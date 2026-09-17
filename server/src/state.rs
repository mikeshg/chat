use crate::config::Config;
use crate::error::AppResult;
use dashmap::DashMap;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use uuid::Uuid;

pub type SharedState = Arc<AppState>;

/// user_id -> (conn_id -> 出站帧发送端)
pub type ConnMap = DashMap<Uuid, DashMap<Uuid, mpsc::UnboundedSender<String>>>;

pub struct AppState {
    pub pool: PgPool,
    pub cfg: Config,
    pub conns: ConnMap,
    /// 一次性 WS ticket -> (user_id, 签发时间)
    pub tickets: DashMap<String, (Uuid, Instant)>,
    /// 简易限流：key -> 时间戳窗口
    pub rate: DashMap<String, Vec<Instant>>,
}

impl AppState {
    pub fn new(pool: PgPool, cfg: Config) -> SharedState {
        Arc::new(Self {
            pool,
            cfg,
            conns: DashMap::new(),
            tickets: DashMap::new(),
            rate: DashMap::new(),
        })
    }

    pub fn send_to_user(&self, uid: Uuid, payload: &str) {
        if let Some(m) = self.conns.get(&uid) {
            for s in m.iter() {
                let _ = s.send(payload.to_string());
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

    pub fn conn_count(&self, uid: Uuid) -> usize {
        self.conns.get(&uid).map(|m| m.len()).unwrap_or(0)
    }

    pub fn issue_ticket(&self, uid: Uuid) -> String {
        let mut buf = [0u8; 32];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut buf);
        let ticket = hex::encode(buf);
        // 惰性清理超过 60s 未消费的 ticket
        self.tickets
            .retain(|_, (_, ts)| ts.elapsed().as_secs() < 60);
        self.tickets.insert(ticket.clone(), (uid, Instant::now()));
        ticket
    }

    /// 好友 ∪ 共群成员 —— 即关心我 presence 的人，也是其 presence 对我可见的人
    pub async fn watchers(&self, uid: Uuid) -> AppResult<Vec<Uuid>> {
        let rows = sqlx::query_scalar::<_, Uuid>(
            "SELECT DISTINCT u FROM (
                 SELECT CASE WHEN user_a = $1 THEN user_b ELSE user_a END AS u
                   FROM friendships WHERE user_a = $1 OR user_b = $1
                 UNION
                 SELECT m2.user_id FROM memberships m1
                   JOIN memberships m2 ON m1.conversation_id = m2.conversation_id
                  WHERE m1.user_id = $1
             ) t WHERE u <> $1",
        )
        .bind(uid)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
