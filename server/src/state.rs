use crate::config::Config;
use crate::error::AppResult;
use dashmap::DashMap;
use sqlx::PgPool;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use uuid::Uuid;

pub type SharedState = Arc<AppState>;

/// 连接出站事件：普通帧，或踢下线（先推 payload 帧再主动断开 socket）
pub enum ConnMsg {
    Frame(String),
    Shutdown(String),
}

/// 单连接出站队列上限。对端读不动（弱网/死连接）时队列满 → 断开，客户端重连后按 seq 补齐。
/// ponytail: 按帧数限，不按字节；大帧多的场景再改字节预算。
pub const CONN_QUEUE: usize = 1024;

/// 单条在线连接：出站事件发送端 + 所属登录会话（老连接可为 None）
pub struct Conn {
    pub tx: mpsc::Sender<ConnMsg>,
    pub sid: Option<Uuid>,
    /// 客户端经 client_state 帧上报的前台活跃态（V2 P3-3）；
    /// 不上报的旧客户端默认 true（保守视为活跃，不误推）
    pub active: AtomicBool,
    /// 出站队列满（或已关）时置位，会话循环看到后断开
    pub stalled: Arc<AtomicBool>,
}

impl Conn {
    /// 非阻塞投递：广播方绝不等慢连接
    fn push(&self, m: ConnMsg) {
        if self.tx.try_send(m).is_err() {
            self.stalled.store(true, Ordering::Relaxed);
        }
    }
}

/// user_id -> (conn_id -> 连接)
pub type ConnMap = DashMap<Uuid, DashMap<Uuid, Conn>>;

/// Agent A01：一条宿主连接。与 Conn 分表登记——宿主连接不计 presence、
/// 不在 send_to_user 的扇出里。
pub struct HostConn {
    pub tx: mpsc::Sender<ConnMsg>,
    /// 防陈旧注销：同 (user, install_id) 重连顶替后，旧连接的清理不得摘掉新连接
    pub conn_id: Uuid,
    /// 宿主所属用户（agent_hosts.user_id 快照，状态广播用）
    pub user_id: Uuid,
    /// 这条连接用的登录会话（ticket 里的 sid）。踢设备 / 改密时按它断开。
    pub sid: Option<Uuid>,
}

/// Agent A03：一次进行中调用的运行时状态（纯内存；调用终结即摘）。
/// 防伪造：宿主的 invocation_event 只受理 host_id 匹配的调用。
pub struct InvRun {
    /// 该调用派给的宿主
    pub host_id: Uuid,
    pub conversation_id: Uuid,
    /// 流式 / 步骤帧的接收者。派发时是「调用会话成员 ∪ 根会话成员」，
    /// 成员变更提交后按当时的成员重算（不在推流路径上查库）。
    pub member_ids: Vec<Uuid>,
    /// 主人 / Agent id（步骤消息与定稿消息的回写需要）
    pub owner: Uuid,
    pub agent_id: Uuid,
    /// 派发时的调用视图快照（状态推进帧沿用，current_step 每次现填）
    pub inv: crate::model::InvocationView,
    /// 生效输出方式：stream / status / steps
    pub mode: String,
    /// 进行中步骤（按开始顺序）：(id, title, kind)
    pub open: Vec<(String, String, String)>,
    /// 已完成步骤（按收尾顺序）
    pub steps: Vec<crate::model::AgentStepView>,
    /// stream 模式待推送的文本增量
    pub stream_buf: String,
    /// 上次向成员推送 agent_stream 的时刻（200ms 限频窗口基准）
    pub last_push: Instant,
    /// 是否已有延迟冲刷任务在跑
    pub flushing: bool,
}

/// 一次性 WS ticket 的内容：签发时的用户、凭据版本与登录会话
pub struct TicketGrant {
    pub uid: Uuid,
    pub session_version: i64,
    pub sid: Option<Uuid>,
    pub issued: Instant,
}

pub struct AppState {
    pub pool: PgPool,
    pub cfg: Config,
    pub conns: ConnMap,
    /// 一次性 WS ticket -> 授权信息
    pub tickets: DashMap<String, TicketGrant>,
    /// 简易限流：key -> 时间戳窗口
    pub rate: DashMap<String, Vec<Instant>>,
    /// 每次在线/离线翻转加一。广播前再核对，避免连上和断开交错时把状态写反。
    epochs: DashMap<Uuid, u64>,
    /// V2 P3-3：(user_id, conversation_id) -> 最近一次离线推送时刻（30s 合并窗口）。
    /// ponytail: 单实例内存状态，重启即丢；重启后短时间内重复推送可接受。
    pub push_last: DashMap<(Uuid, Uuid), Instant>,
    /// V2 P3-3：WS 入站静默超时（毫秒），默认 75s ≈ 2.5 个 ping 周期；测试可调小
    pub ws_idle_timeout_ms: AtomicU64,
    /// V2 P3-3：WS ping 周期（毫秒），默认 30s；与超时一起调小做心跳测试
    pub ws_ping_interval_ms: AtomicU64,
    /// V2 P4-1：维护模式。为 true 时除正在执行的恢复请求外所有 /v1 请求 503（含 WS 升级）。
    pub maintenance: AtomicBool,
    /// V2 P4-1：备份/恢复互斥锁（同一时间只允许一个备份生成或一次恢复进行）。
    pub backup_busy: AtomicBool,
    /// Agent A01：host_id -> 宿主连接（在线即有此表项）
    pub hosts: DashMap<Uuid, HostConn>,
    /// Agent A04：排队超时清扫间隔（毫秒），默认 30s。
    /// 清扫循环每小段重读这个值，测试调小后不用等完原来的 30 秒。
    pub queue_sweep_interval_ms: AtomicU64,
    /// Agent A23：宿主离线后，进行中的调用再等多久判 host_disconnected（毫秒）。默认 10 分钟。
    /// 和排队清扫共用一个循环；测试调小。起点在 agent_hosts.offline_since，重启后仍在。
    pub host_offline_grace_ms: AtomicU64,
    /// 下发 invocation_start 和 invocation_cancel 共用这把锁，避免取消帧早于启动帧。
    /// 全局锁：只包一次状态查询和一次非阻塞发送。吞吐成问题时按调用 id 分锁。
    pub inv_gate: tokio::sync::Mutex<()>,
    /// Agent A03：invocation_id -> 进行中调用的运行时状态
    pub inv_runs: DashMap<Uuid, InvRun>,
    /// Agent A17：定时调用扫描间隔（毫秒），默认 30s。测试调小后不用等完原来的 30 秒。
    pub schedule_sweep_interval_ms: AtomicU64,
    /// Agent A07：登记表请求 request_id → 等宿主回 registry_result。超时从这里摘掉。
    pub registry_wait: DashMap<Uuid, tokio::sync::oneshot::Sender<serde_json::Value>>,
}

impl AppState {
    pub fn new(pool: PgPool, cfg: Config) -> SharedState {
        let st = Arc::new(Self {
            pool,
            cfg,
            conns: DashMap::new(),
            tickets: DashMap::new(),
            rate: DashMap::new(),
            epochs: DashMap::new(),
            push_last: DashMap::new(),
            ws_idle_timeout_ms: AtomicU64::new(75_000),
            ws_ping_interval_ms: AtomicU64::new(30_000),
            maintenance: AtomicBool::new(false),
            backup_busy: AtomicBool::new(false),
            hosts: DashMap::new(),
            queue_sweep_interval_ms: AtomicU64::new(30_000),
            host_offline_grace_ms: AtomicU64::new(600_000),
            inv_gate: tokio::sync::Mutex::new(()),
            inv_runs: DashMap::new(),
            schedule_sweep_interval_ms: AtomicU64::new(30_000),
            registry_wait: DashMap::new(),
        });
        crate::agents::spawn_queue_sweeper(st.clone());
        crate::schedules::spawn_sweeper(st.clone());
        st
    }

    fn bump_epoch(&self, uid: Uuid) -> u64 {
        let mut n = self.epochs.entry(uid).or_insert(0);
        *n += 1;
        *n
    }

    pub fn epoch_is(&self, uid: Uuid, epoch: u64) -> bool {
        self.epochs.get(&uid).is_some_and(|n| *n == epoch)
    }

    /// 登记连接。返回 Some(epoch) 表示这是该用户的第一条连接（离线 → 在线）。
    pub fn register_conn(
        &self,
        uid: Uuid,
        conn_id: Uuid,
        tx: mpsc::Sender<ConnMsg>,
        sid: Option<Uuid>,
        stalled: Arc<AtomicBool>,
    ) -> Option<u64> {
        let first = {
            let bucket = self.conns.entry(uid).or_default();
            let first = bucket.is_empty();
            bucket.insert(
                conn_id,
                Conn {
                    tx,
                    sid,
                    active: AtomicBool::new(true),
                    stalled,
                },
            );
            first
        };
        first.then(|| self.bump_epoch(uid))
    }

    /// 移除连接。返回 Some(epoch) 表示这是最后一条连接（在线 → 离线）。
    pub fn unregister_conn(&self, uid: Uuid, conn_id: Uuid) -> Option<u64> {
        let now_empty = {
            let bucket = self.conns.get_mut(&uid)?;
            if bucket.remove(&conn_id).is_none() {
                return None;
            }
            bucket.is_empty()
        };
        if !now_empty {
            return None;
        }
        let still_empty = self.conns.get(&uid).is_some_and(|bucket| bucket.is_empty());
        if !still_empty {
            return None;
        }
        let epoch = self.bump_epoch(uid);
        self.conns.remove_if(&uid, |_, conns| conns.is_empty());
        Some(epoch)
    }

    pub fn send_to_user(&self, uid: Uuid, payload: &str) {
        if let Some(m) = self.conns.get(&uid) {
            for s in m.iter() {
                s.push(ConnMsg::Frame(payload.to_string()));
            }
        }
    }

    pub fn send_to_users(&self, uids: &[Uuid], payload: &str) {
        for u in uids {
            self.send_to_user(*u, payload);
        }
    }

    pub fn is_online(&self, uid: Uuid) -> bool {
        self.conns.get(&uid).map(|m| !m.is_empty()).unwrap_or(false)
    }

    /// 踢下线：给该用户所有连接发 force_logout 并主动断开 socket，同时摘掉连接表。
    /// 返回 Some(epoch) 表示刚才还在线，调用方负责广播离线。
    /// 断线清理看到连接已不在表里，不会再广播一次。
    /// 该用户的宿主连接一并断开（A08：宿主跟随登录会话，禁用 / 恢复备份走这里）。
    pub fn kick_user(&self, uid: Uuid, reason: &str) -> Option<u64> {
        self.disconnect_hosts(uid, |_| false, reason);
        let Some((_, bucket)) = self.conns.remove(&uid) else {
            return None;
        };
        let frame = serde_json::json!({
            "type": "force_logout",
            "reason": reason,
        })
        .to_string();
        let mut had = false;
        for conn in bucket.iter() {
            had = true;
            conn.push(ConnMsg::Shutdown(frame.clone()));
        }
        had.then(|| self.bump_epoch(uid))
    }

    /// 踢掉满足条件的连接（先收 force_logout 帧再断开）。
    /// 返回 Some(epoch) 仅当踢完后该用户已无连接（在线 → 离线），由调用方广播离线；
    /// 仍有其他连接在线时返回 None，绝不能广播离线。
    fn kick_matching(
        &self,
        uid: Uuid,
        reason: &str,
        keep: impl Fn(Option<Uuid>) -> bool,
    ) -> Option<u64> {
        let frame = serde_json::json!({
            "type": "force_logout",
            "reason": reason,
        })
        .to_string();
        let mut kicked = false;
        let emptied = {
            let bucket = self.conns.get_mut(&uid)?;
            let ids: Vec<Uuid> = bucket
                .iter()
                .filter(|e| !keep(e.sid))
                .map(|e| *e.key())
                .collect();
            for id in ids {
                if let Some((_, conn)) = bucket.remove(&id) {
                    kicked = true;
                    conn.push(ConnMsg::Shutdown(frame.clone()));
                }
            }
            kicked && bucket.is_empty()
        };
        if !emptied {
            return None;
        }
        // 与 unregister_conn 同样的二次确认：可能恰好有新连接注册进来
        let still_empty = self.conns.get(&uid).is_some_and(|b| b.is_empty());
        if !still_empty {
            return None;
        }
        let epoch = self.bump_epoch(uid);
        self.conns.remove_if(&uid, |_, conns| conns.is_empty());
        Some(epoch)
    }

    /// 踢掉某个登录会话的全部连接（设备被移出），含该会话上的宿主。
    pub fn kick_session(&self, uid: Uuid, sid: Uuid, reason: &str) -> Option<u64> {
        self.disconnect_hosts(uid, |s| s != Some(sid), reason);
        self.kick_matching(uid, reason, |s| s != Some(sid))
    }

    /// 踢掉除 keep 以外登录会话的全部连接（改密/一键退出其他设备），含对应宿主。
    /// keep=None（老 token 发起的请求）：只踢带 sid 的连接，无 sid 的老连接保留，
    /// 由 session_version 变化在下一帧自然断开。
    pub fn kick_other_sessions(&self, uid: Uuid, keep: Option<Uuid>, reason: &str) -> Option<u64> {
        self.disconnect_hosts(uid, |s| s == keep, reason);
        self.kick_matching(uid, reason, |s| s == keep)
    }

    /// 某个登录会话当前是否有在线连接（设备列表的 online 点）
    pub fn session_online(&self, uid: Uuid, sid: Uuid) -> bool {
        self.conns
            .get(&uid)
            .is_some_and(|m| m.iter().any(|c| c.sid == Some(sid)))
    }

    /// 连接是否仍在连接表里（被踢后旧任务不得再受理任何帧）
    pub fn conn_alive(&self, uid: Uuid, conn_id: Uuid) -> bool {
        self.conns
            .get(&uid)
            .is_some_and(|m| m.contains_key(&conn_id))
    }

    /// client_state 帧更新连接的前台活跃标记（V2 P3-3）
    pub fn set_conn_active(&self, uid: Uuid, conn_id: Uuid, active: bool) {
        if let Some(m) = self.conns.get(&uid) {
            if let Some(c) = m.get(&conn_id) {
                c.active.store(active, Ordering::Relaxed);
            }
        }
    }

    /// 是否至少一条连接处于前台活跃（决定离线推送是否触发；无连接 = false）
    pub fn has_active_conn(&self, uid: Uuid) -> bool {
        self.conns
            .get(&uid)
            .is_some_and(|m| m.iter().any(|c| c.active.load(Ordering::Relaxed)))
    }

    pub fn conn_count(&self, uid: Uuid) -> usize {
        self.conns.get(&uid).map(|m| m.len()).unwrap_or(0)
    }

    // ---------- Agent A01：宿主连接表 ----------

    /// 登记宿主连接；同 host 已有连接时给旧连接发 host_superseded 并断开。
    /// sid 是这条连接的登录会话，供改密 / 移出设备时对上号。
    pub fn register_host(
        &self,
        host_id: Uuid,
        user_id: Uuid,
        conn_id: Uuid,
        sid: Option<Uuid>,
        tx: mpsc::Sender<ConnMsg>,
    ) {
        if let Some(old) = self.hosts.insert(
            host_id,
            HostConn {
                tx,
                conn_id,
                user_id,
                sid,
            },
        ) {
            let _ = old.tx.try_send(ConnMsg::Shutdown(
                serde_json::json!({"type": "host_superseded"}).to_string(),
            ));
        }
    }

    /// 移除宿主连接；仅当表里的仍是这条连接时才移除（顶替保护）。
    /// 返回 true = 本连接确实在册，调用方负责离线收尾。
    pub fn unregister_host(&self, host_id: Uuid, conn_id: Uuid) -> bool {
        self.hosts
            .remove_if(&host_id, |_, c| c.conn_id == conn_id)
            .is_some()
    }

    /// 宿主当前在线（连接表里有登记）
    pub fn host_online(&self, host_id: Uuid) -> bool {
        self.hosts.contains_key(&host_id)
    }

    /// 添加 Agent 用：这条在线连接的登录会话是不是这次请求的会话。
    /// 没有 sid、电脑离线、或会话对不上，都不是本机。
    pub fn host_is_local_session(&self, host_id: Uuid, sid: Option<Uuid>) -> bool {
        let Some(sid) = sid else {
            return false;
        };
        self.hosts.get(&host_id).is_some_and(|c| c.sid == Some(sid))
    }

    /// 断开该用户全部宿主（使用权收回）。不摘连接表，会话循环自己收尾并标离线。
    pub fn disconnect_user_hosts(&self, uid: Uuid) {
        self.disconnect_hosts(uid, |_| false, "agent_access_revoked");
    }

    /// keep 返回 true 的宿主留下。先发关闭帧，并立刻给主人推 online=false
    /// （会话循环随后还会再推一次；GET 要等循环摘表才变离线）。
    fn disconnect_hosts(&self, uid: Uuid, keep: impl Fn(Option<Uuid>) -> bool, reason: &str) {
        let frame = serde_json::json!({
            "type": "force_logout",
            "reason": reason,
        })
        .to_string();
        let ids: Vec<Uuid> = self
            .hosts
            .iter()
            .filter(|e| e.user_id == uid && !keep(e.sid))
            .map(|e| *e.key())
            .collect();
        for host_id in ids {
            let Some(conn) = self.hosts.get(&host_id) else {
                continue;
            };
            if conn.user_id != uid || keep(conn.sid) {
                continue;
            }
            let _ = conn.tx.try_send(ConnMsg::Shutdown(frame.clone()));
            drop(conn);
            let status = serde_json::json!({
                "type": "agent_host_status",
                "host_id": host_id,
                "online": false,
            })
            .to_string();
            self.send_to_user(uid, &status);
        }
    }

    /// 向宿主连接投递一帧；宿主离线或队列满返回 false
    pub fn send_to_host(&self, host_id: Uuid, payload: &str) -> bool {
        self.hosts
            .get(&host_id)
            .is_some_and(|c| c.tx.try_send(ConnMsg::Frame(payload.to_string())).is_ok())
    }

    pub fn issue_ticket(&self, uid: Uuid, session_version: i64, sid: Option<Uuid>) -> String {
        let mut buf = [0u8; 32];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut buf);
        let ticket = hex::encode(buf);
        // 惰性清理超过 60s 未消费的 ticket
        self.tickets
            .retain(|_, t| t.issued.elapsed().as_secs() < 60);
        self.tickets.insert(
            ticket.clone(),
            TicketGrant {
                uid,
                session_version,
                sid,
                issued: Instant::now(),
            },
        );
        ticket
    }

    /// 好友 ∪ 共群成员 —— 即关心我 presence 的人，也是其 presence 对我可见的人。
    /// 注意排除 DM：删好友后保留的只读 DM 不构成 presence 可见性。
    pub async fn watchers(&self, uid: Uuid) -> AppResult<Vec<Uuid>> {
        let rows = sqlx::query_scalar::<_, Uuid>(
            "SELECT DISTINCT u FROM (
                 SELECT CASE WHEN user_a = $1 THEN user_b ELSE user_a END AS u
                   FROM friendships WHERE user_a = $1 OR user_b = $1
                 UNION
                 SELECT m2.user_id FROM memberships m1
                   JOIN memberships m2 ON m1.conversation_id = m2.conversation_id
                   JOIN conversations c ON c.id = m1.conversation_id
                  WHERE m1.user_id = $1 AND c.type = 'group'
             ) t WHERE u <> $1",
        )
        .bind(uid)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
