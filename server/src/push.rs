//! ntfy 离线推送（V2 P3-3）。
//! 触发点收敛在 ws::broadcast_posted：新消息落库后对每个成员判定是否补一条 ntfy 通知。
//! 推送服务器地址只能由管理员配置（server_settings），用户只持有服务端生成的随机主题——
//! 不接受用户提供的 URL，防 SSRF。

use crate::auth;
use crate::domain;
use crate::error::{is_unique_violation, AppError, AppResult};
use crate::model::MessageView;
use crate::settings::{self, ServerSettings};
use crate::state::SharedState;
use axum::{extract::State, http::HeaderMap, Json};
use chrono::{DateTime, Utc};
use dashmap::mapref::entry::Entry;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// ntfy 主题：chat-<24 位小写字母数字>
const TOPIC_ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const TOPIC_LEN: usize = 24;

/// 同一 (用户, 会话) 的推送合并窗口
const COALESCE: Duration = Duration::from_secs(30);

/// 推送 HTTP 超时
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

fn gen_topic() -> String {
    // 只取 < 252（= 36*7）的字节再取模，消除模偏置
    let mut out = String::with_capacity(5 + TOPIC_LEN);
    out.push_str("chat-");
    let mut buf = [0u8; 48];
    while out.len() < 5 + TOPIC_LEN {
        OsRng.fill_bytes(&mut buf);
        for &b in buf.iter() {
            if b < 252 {
                out.push(TOPIC_ALPHABET[(b % 36) as usize] as char);
                if out.len() == 5 + TOPIC_LEN {
                    break;
                }
            }
        }
    }
    out
}

struct PushRow {
    enabled: bool,
    preview: bool,
    topic: String,
}

#[derive(Debug, Serialize)]
pub struct PushView {
    /// 服务端已配置 ntfy 地址
    pub available: bool,
    pub enabled: bool,
    /// 通知是否带消息内容（公共 ntfy 服务器上主题可被猜到的人看到，默认关）
    pub preview: bool,
    /// {push_ntfy_base}/{topic}；未配置推送时为 null
    pub subscribe_url: Option<String>,
}

async fn fetch_row(pool: &PgPool, uid: Uuid) -> AppResult<Option<PushRow>> {
    let row = sqlx::query_as::<_, (bool, bool, String)>(
        "SELECT enabled, preview, topic FROM push_settings WHERE user_id = $1",
    )
    .bind(uid)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(enabled, preview, topic)| PushRow {
        enabled,
        preview,
        topic,
    }))
}

/// 首次访问生成主题建行；并发/主题撞车（唯一键）时重试
async fn ensure_row(pool: &PgPool, uid: Uuid) -> AppResult<PushRow> {
    for _ in 0..5 {
        let res = sqlx::query(
            "INSERT INTO push_settings (user_id, topic) VALUES ($1, $2)
             ON CONFLICT (user_id) DO NOTHING",
        )
        .bind(uid)
        .bind(gen_topic())
        .execute(pool)
        .await;
        match res {
            Err(e) if is_unique_violation(&e) => continue,
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        if let Some(row) = fetch_row(pool, uid).await? {
            return Ok(row);
        }
    }
    Err(AppError::internal("push_settings insert failed"))
}

async fn view_of(st: &SharedState, row: &PushRow) -> AppResult<PushView> {
    let s = settings::load(&st.pool).await?;
    let available = !s.push_ntfy_base.is_empty();
    Ok(PushView {
        available,
        enabled: row.enabled,
        preview: row.preview,
        subscribe_url: available.then(|| format!("{}/{}", s.push_ntfy_base, row.topic)),
    })
}

/// GET /users/me/push：无行时建默认行（生成主题）
pub async fn get_push(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<PushView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    let row = ensure_row(&st.pool, uid).await?;
    Ok(Json(view_of(&st, &row).await?))
}

#[derive(Debug, Deserialize)]
pub struct PutPushReq {
    pub enabled: Option<bool>,
    pub preview: Option<bool>,
}

/// PUT /users/me/push：缺省字段不改；enabled=true 要求服务端已配置 ntfy
pub async fn put_push(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<PutPushReq>,
) -> AppResult<Json<PushView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    if req.enabled == Some(true) && settings::load(&st.pool).await?.push_ntfy_base.is_empty() {
        return Err(AppError::bad_request("管理员未配置推送服务"));
    }
    ensure_row(&st.pool, uid).await?;
    sqlx::query(
        "UPDATE push_settings
            SET enabled = COALESCE($2, enabled),
                preview = COALESCE($3, preview),
                updated_at = now()
          WHERE user_id = $1",
    )
    .bind(uid)
    .bind(req.enabled)
    .bind(req.preview)
    .execute(&st.pool)
    .await?;
    let row = fetch_row(&st.pool, uid).await?.unwrap();
    Ok(Json(view_of(&st, &row).await?))
}

/// POST /users/me/push/rotate：换新主题（旧主题作废，用户需在 ntfy App 里重新订阅）
pub async fn rotate_push(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<PushView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    for _ in 0..5 {
        let res = sqlx::query(
            "UPDATE push_settings SET topic = $2, updated_at = now() WHERE user_id = $1",
        )
        .bind(uid)
        .bind(gen_topic())
        .execute(&st.pool)
        .await;
        match res {
            Ok(r) if r.rows_affected() == 0 => {
                // 从未访问过推送设置：先建行，下一轮再 UPDATE
                ensure_row(&st.pool, uid).await?;
            }
            Ok(_) => break,
            Err(e) if is_unique_violation(&e) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let row = fetch_row(&st.pool, uid)
        .await?
        .ok_or_else(|| AppError::internal("push_settings rotate failed"))?;
    Ok(Json(view_of(&st, &row).await?))
}

/// POST /users/me/push/test：立即发一条测试推送（不受 30s 合并限制），失败 502 带原因
pub async fn test_push(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<PushView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    let s = settings::load(&st.pool).await?;
    if s.push_ntfy_base.is_empty() {
        return Err(AppError::bad_request("管理员未配置推送服务"));
    }
    let row = ensure_row(&st.pool, uid).await?;
    let payload = NtfyPayload {
        topic: &row.topic,
        title: "Chat",
        message: "测试推送：看到这条通知说明推送通道已连通",
        priority: 4,
        tags: &["speech_balloon"],
        click: public_url(&s),
    };
    send(&s, &payload).await.map_err(AppError::bad_gateway)?;
    Ok(Json(view_of(&st, &row).await?))
}

// ---------- 发送 ----------

/// ntfy JSON 发布请求体（POST {base}，UTF-8 JSON 天然绕开头部非 ASCII 编码问题）。
/// priority 用 ntfy 的 1-5 整数刻度：3 = default，4 = high。
#[derive(Serialize)]
struct NtfyPayload<'a> {
    topic: &'a str,
    title: &'a str,
    message: &'a str,
    priority: u8,
    tags: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    click: Option<&'a str>,
}

fn public_url(s: &ServerSettings) -> Option<&str> {
    (!s.public_url.is_empty()).then_some(s.public_url.as_str())
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            // 内网回环假 ntfy（测试）也要能直连，不走系统代理
            .no_proxy()
            .build()
            .expect("build reqwest client")
    })
}

/// 投递一条 ntfy 通知；失败返回原因串（调用方记日志或转 502），不重试。
async fn send(s: &ServerSettings, p: &NtfyPayload<'_>) -> Result<(), String> {
    let mut req = client().post(&s.push_ntfy_base).json(p);
    if !s.push_ntfy_token.is_empty() {
        req = req.bearer_auth(&s.push_ntfy_token);
    }
    let resp = req.send().await.map_err(|e| format!("推送请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("ntfy 返回 {}", resp.status()));
    }
    Ok(())
}

// ---------- 新消息触发（挂在 ws::broadcast_posted） ----------

/// 推送摘要：与客户端通知一致（图片/文件/语音/投票占位，文本截前 100 字）
fn excerpt(msg: &MessageView) -> String {
    if let Some(a) = &msg.attachment {
        if msg.kind == "image" {
            return "[图片]".into();
        }
        if msg.kind == "audio" || a.duration_ms.is_some() {
            return "[语音]".into();
        }
        return format!("[文件] {}", a.file_name);
    }
    match msg.kind.as_str() {
        "poll" => msg
            .poll
            .as_ref()
            .map(|p| format!("[投票] {}", p.question))
            .unwrap_or_else(|| msg.content.chars().take(100).collect()),
        _ => msg.content.chars().take(100).collect(),
    }
}

/// 新消息离线推送入：整个判定与发送都在 spawned 任务里，绝不在广播路径上等待。
pub fn schedule(st: &SharedState, conv: Uuid, posted: &domain::Posted) {
    let st = st.clone();
    let message = posted.message.clone();
    tokio::spawn(async move {
        if let Err(e) = dispatch(&st, conv, &message).await {
            tracing::warn!(conv = %conv, error = ?e, "push dispatch failed");
        }
    });
}

async fn dispatch(st: &SharedState, conv: Uuid, msg: &MessageView) -> AppResult<()> {
    let s = settings::load(&st.pool).await?;
    if s.push_ntfy_base.is_empty() {
        return Ok(());
    }
    // 候选成员一条 SQL 取齐：已开推送（排除发送者）+ 会话偏好（无行 = 未静音未归档）
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            bool,
            bool,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
        ),
    >(
        "SELECT ps.user_id, ps.topic, ps.preview,
                COALESCE(cp.mute_forever, false), cp.muted_until, cp.archived_at
           FROM push_settings ps
           JOIN memberships m ON m.user_id = ps.user_id AND m.conversation_id = $1
           LEFT JOIN conversation_preferences cp
             ON cp.conversation_id = $1 AND cp.user_id = ps.user_id
          WHERE ps.enabled AND ps.user_id <> $2",
    )
    .bind(conv)
    .bind(msg.sender_id)
    .fetch_all(&st.pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    // ponytail: 合并窗口是单实例内存状态，重启丢失（重启后可能多推一次，可接受）；
    // 超阈值时惰性清理防无界增长
    if st.push_last.len() > 4096 {
        st.push_last.retain(|_, t| t.elapsed() < COALESCE * 4);
    }

    let title = match domain::conv_kind(&st.pool, conv).await?.as_deref() {
        Some("group") => {
            sqlx::query_scalar::<_, Option<String>>("SELECT name FROM conversations WHERE id = $1")
                .bind(conv)
                .fetch_one(&st.pool)
                .await?
                .unwrap_or_else(|| "群聊".into())
        }
        _ => msg.sender_name.clone(),
    };

    let now = Utc::now();
    for (uid, topic, preview, mute_forever, muted_until, archived_at) in rows {
        // 至少一条活跃连接 = 用户在线且在前台，不需要离线推送
        if st.has_active_conn(uid) {
            continue;
        }
        let mentioned = msg.mentions.contains(&uid);
        // 静音/归档只放行 @本人（@已在写消息事务内解除归档，能走到这里主要靠静音判断）
        let muted = mute_forever || muted_until.is_some_and(|t| t > now);
        if (muted || archived_at.is_some()) && !mentioned {
            continue;
        }
        // 30s 合并：entry 原子占位，并发任务只有一个赢家
        match st.push_last.entry((uid, conv)) {
            Entry::Occupied(e) if e.get().elapsed() < COALESCE => continue,
            Entry::Occupied(mut e) => {
                e.insert(Instant::now());
            }
            Entry::Vacant(e) => {
                e.insert(Instant::now());
            }
        }
        let body = if preview {
            format!(
                "{}{}: {}",
                if mentioned { "[有人@我] " } else { "" },
                msg.sender_name,
                excerpt(msg)
            )
        } else if mentioned {
            "有人@了你".to_string()
        } else {
            "你收到一条新消息".to_string()
        };
        let p = NtfyPayload {
            topic: &topic,
            title: &title,
            message: &body,
            priority: if mentioned { 4 } else { 3 },
            tags: &["speech_balloon"],
            click: public_url(&s),
        };
        if let Err(e) = send(&s, &p).await {
            tracing::warn!(user = %uid, error = %e, "ntfy push failed");
        }
    }
    Ok(())
}
