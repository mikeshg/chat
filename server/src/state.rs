use crate::config::Config;
use crate::error::AppResult;
use dashmap::DashMap;
use sqlx::PgPool;
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

/// user_id -> (conn_id -> 出站事件发送端)
pub type ConnMap = DashMap<Uuid, DashMap<Uuid, mpsc::UnboundedSender<ConnMsg>>>;

/// 一次性 WS ticket 的内容：签发时的用户与凭据版本
pub struct TicketGrant {
    pub uid: Uuid,
    pub session_version: i64,
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
}

impl AppState {
    pub fn new(pool: PgPool, cfg: Config) -> SharedState {
        Arc::new(Self {
            pool,
            cfg,
            conns: DashMap::new(),
            tickets: DashMap::new(),
            rate: DashMap::new(),
            epochs: DashMap::new(),
        })
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
        tx: mpsc::UnboundedSender<ConnMsg>,
    ) -> Option<u64> {
        let first = {
            let bucket = self.conns.entry(uid).or_default();
            let first = bucket.is_empty();
            bucket.insert(conn_id, tx);
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
                let _ = s.send(ConnMsg::Frame(payload.to_string()));
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
    pub fn kick_user(&self, uid: Uuid, reason: &str) -> Option<u64> {
        let Some((_, bucket)) = self.conns.remove(&uid) else {
            return None;
        };
        let frame = serde_json::json!({
            "type": "force_logout",
            "reason": reason,
        })
        .to_string();
        let mut had = false;
        for sender in bucket.iter() {
            had = true;
            let _ = sender.send(ConnMsg::Shutdown(frame.clone()));
        }
        had.then(|| self.bump_epoch(uid))
    }

    /// 连接是否仍在连接表里（被踢后旧任务不得再受理任何帧）
    pub fn conn_alive(&self, uid: Uuid, conn_id: Uuid) -> bool {
        self.conns
            .get(&uid)
            .is_some_and(|m| m.contains_key(&conn_id))
    }

    pub fn conn_count(&self, uid: Uuid) -> usize {
        self.conns.get(&uid).map(|m| m.len()).unwrap_or(0)
    }

    pub fn issue_ticket(&self, uid: Uuid, session_version: i64) -> String {
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
