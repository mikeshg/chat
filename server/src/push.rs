//! ntfy 离线推送（V2 P3-3）。
//! 触发点：ws::broadcast_posted（新消息 → 推）与已读推进（→ 清掉手机上的通知）。
//! 推送服务器地址只能由管理员配置（server_settings）；用户的每台手机持有一个服务端生成的随机主题——
//! 不接受用户提供的 URL，防 SSRF。
//! 每条通知带 sequence_id = 会话 id：同一会话在手机上只占一条通知，新消息原地更新；
//! 任一端读过该会话就按 sequence_id 清掉（ntfy 服务端 ≥ 2.16、Android App ≥ 1.22.2，旧版忽略）。

use crate::auth;
use crate::domain;
use crate::error::{is_unique_violation, AppError, AppResult};
use crate::model::MessageView;
use crate::settings::{self, ServerSettings};
use crate::state::SharedState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use dashmap::mapref::entry::Entry;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// ntfy 主题：chat-<24 位小写字母数字>
const TOPIC_ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const TOPIC_LEN: usize = 24;

/// 每人最多登记的手机数
const MAX_DEVICES: i64 = 10;

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

#[derive(Debug, Serialize)]
pub struct PushDeviceView {
    pub id: Uuid,
    pub name: String,
    /// {push_ntfy_base}/{topic}；未配置推送时为 null
    pub subscribe_url: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct PushView {
    /// 服务端已配置 ntfy 地址
    pub available: bool,
    pub enabled: bool,
    /// 通知是否带消息内容（公共 ntfy 服务器上主题可被猜到的人看到，默认关）
    pub preview: bool,
    pub devices: Vec<PushDeviceView>,
}

async fn view_of(st: &SharedState, uid: Uuid) -> AppResult<Json<PushView>> {
    let s = settings::load(&st.pool).await?;
    let available = !s.push_ntfy_base.is_empty();
    let (enabled, preview) = sqlx::query_as::<_, (bool, bool)>(
        "SELECT enabled, preview FROM push_settings WHERE user_id = $1",
    )
    .bind(uid)
    .fetch_optional(&st.pool)
    .await?
    .unwrap_or((false, false));
    let devices = sqlx::query_as::<_, (Uuid, String, String, DateTime<Utc>)>(
        "SELECT id, name, topic, created_at FROM push_devices WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(uid)
    .fetch_all(&st.pool)
    .await?
    .into_iter()
    .map(|(id, name, topic, created_at)| PushDeviceView {
        id,
        name,
        subscribe_url: available.then(|| format!("{}/{}", s.push_ntfy_base, topic)),
        created_at,
    })
    .collect();
    Ok(Json(PushView {
        available,
        enabled,
        preview,
        devices,
    }))
}

fn require_configured(s: &ServerSettings) -> AppResult<()> {
    if s.push_ntfy_base.is_empty() {
        return Err(AppError::bad_request("管理员未配置推送服务"));
    }
    Ok(())
}

/// GET /users/me/push
pub async fn get_push(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<PushView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    view_of(&st, uid).await
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
    if req.enabled == Some(true) {
        require_configured(&settings::load(&st.pool).await?)?;
    }
    sqlx::query(
        "INSERT INTO push_settings (user_id, enabled, preview)
         VALUES ($1, COALESCE($2, false), COALESCE($3, false))
         ON CONFLICT (user_id) DO UPDATE
            SET enabled = COALESCE($2, push_settings.enabled),
                preview = COALESCE($3, push_settings.preview),
                updated_at = now()",
    )
    .bind(uid)
    .bind(req.enabled)
    .bind(req.preview)
    .execute(&st.pool)
    .await?;
    view_of(&st, uid).await
}

#[derive(Debug, Deserialize)]
pub struct AddDeviceReq {
    pub name: String,
}

/// POST /users/me/push/devices：登记一台手机，生成它自己的订阅主题
pub async fn add_device(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<AddDeviceReq>,
) -> AppResult<Json<PushView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    require_configured(&settings::load(&st.pool).await?)?;
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 50 {
        return Err(AppError::bad_request("设备名 1-50 个字"));
    }
    // ponytail: 先数再插，并发连点可能多出一台；无害
    let n = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM push_devices WHERE user_id = $1")
        .bind(uid)
        .fetch_one(&st.pool)
        .await?;
    if n >= MAX_DEVICES {
        return Err(AppError::bad_request(format!("最多 {MAX_DEVICES} 台设备")));
    }
    // 主题撞车（唯一键）时换一个重试
    for _ in 0..5 {
        let res = sqlx::query("INSERT INTO push_devices (user_id, name, topic) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(name)
            .bind(gen_topic())
            .execute(&st.pool)
            .await;
        match res {
            Err(e) if is_unique_violation(&e) => continue,
            Err(e) => return Err(e.into()),
            Ok(_) => return view_of(&st, uid).await,
        }
    }
    Err(AppError::internal("push_devices insert failed"))
}

/// DELETE /users/me/push/devices/{id}：移除一台手机，它的主题立即不再收到推送
pub async fn delete_device(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PushView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    let r = sqlx::query("DELETE FROM push_devices WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(uid)
        .execute(&st.pool)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::not_found("设备不存在"));
    }
    view_of(&st, uid).await
}

/// POST /users/me/push/devices/{id}/test：给这台手机立即发一条测试推送，失败 502 带原因
pub async fn test_device(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PushView>> {
    let uid = auth::require_user(&st, &headers).await?.id;
    let s = settings::load(&st.pool).await?;
    require_configured(&s)?;
    let topic = sqlx::query_scalar::<_, String>(
        "SELECT topic FROM push_devices WHERE id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(uid)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::not_found("设备不存在"))?;
    let payload = NtfyPayload {
        topic: &topic,
        title: "Chat",
        message: "测试推送：看到这条通知说明推送通道已连通",
        priority: 4,
        tags: &["speech_balloon"],
        click: public_url(&s),
        sequence_id: None,
    };
    send(&s, &payload).await.map_err(AppError::bad_gateway)?;
    view_of(&st, uid).await
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
    #[serde(skip_serializing_if = "Option::is_none")]
    sequence_id: Option<&'a str>,
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

/// 发请求（带管理员配置的令牌）；失败返回原因串（调用方记日志或转 502），不重试。
async fn deliver(s: &ServerSettings, mut req: reqwest::RequestBuilder) -> Result<(), String> {
    if !s.push_ntfy_token.is_empty() {
        req = req.bearer_auth(&s.push_ntfy_token);
    }
    let resp = req.send().await.map_err(|e| format!("推送请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("ntfy 返回 {}", resp.status()));
    }
    Ok(())
}

async fn send(s: &ServerSettings, p: &NtfyPayload<'_>) -> Result<(), String> {
    deliver(s, client().post(&s.push_ntfy_base).json(p)).await
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
        // 聊天记录：摘要恒为「[聊天记录] 标题」，缺预览时只回落到固定前缀，
        // 永不落到正文兜底行（MF1-C9）
        "chat_record" => msg
            .record_preview
            .as_ref()
            .map(|p| format!("[聊天记录] {}", p.title))
            .unwrap_or_else(|| "[聊天记录]".into()),
        "poll" => msg
            .poll
            .as_ref()
            .map(|p| format!("[投票] {}", p.question))
            .unwrap_or_else(|| msg.content.chars().take(100).collect()),
        _ => msg.content.chars().take(100).collect(),
    }
}

/// 新消息离线推送入口：整个判定与发送都在 spawned 任务里，绝不在广播路径上等待。
pub fn schedule(st: &SharedState, conv: Uuid, posted: &domain::Posted) {
    // 发送者的已读已推进到这条：清掉他手机上这个会话的通知
    clear(st, posted.message.sender_id, conv);
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
    // 候选成员一条 SQL 取齐：已开推送且有手机（排除发送者）+ 未读数 + 会话偏好（无行 = 未静音未归档）。
    // 不看对方是否在线/前台：读过后会把通知清掉。
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            bool,
            i64,
            bool,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Vec<String>,
        ),
    >(
        "SELECT ps.user_id, ps.preview, c.last_seq - m.last_read_seq,
                COALESCE(cp.mute_forever, false), cp.muted_until, cp.archived_at,
                array_agg(pd.topic)
           FROM push_settings ps
           JOIN push_devices pd ON pd.user_id = ps.user_id
           JOIN memberships m ON m.user_id = ps.user_id AND m.conversation_id = $1
           JOIN conversations c ON c.id = $1
           LEFT JOIN conversation_preferences cp
             ON cp.conversation_id = $1 AND cp.user_id = ps.user_id
          WHERE ps.enabled AND ps.user_id <> $2
          GROUP BY ps.user_id, ps.preview, c.last_seq, m.last_read_seq,
                   cp.mute_forever, cp.muted_until, cp.archived_at",
    )
    .bind(conv)
    .bind(msg.sender_id)
    .fetch_all(&st.pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    // 读过的会话已从表里移除；超阈值时清掉一天前的陈旧记录防无界增长
    if st.push_last.len() > 4096 {
        st.push_last
            .retain(|_, t| t.elapsed() < Duration::from_secs(86_400));
    }
    let window = Duration::from_millis(st.push_coalesce_ms.load(Ordering::Relaxed));

    let sender_display = msg.sender_name.clone();
    let name = match domain::conv_kind(&st.pool, conv).await?.as_deref() {
        Some("group") => {
            sqlx::query_scalar::<_, Option<String>>("SELECT name FROM conversations WHERE id = $1")
                .bind(conv)
                .fetch_one(&st.pool)
                .await?
                .unwrap_or_else(|| "群聊".into())
        }
        _ => sender_display.clone(),
    };
    let seq_id = conv.to_string();

    let now = Utc::now();
    for (uid, preview, unread, mute_forever, muted_until, archived_at, topics) in rows {
        let mentioned = msg.mentions.contains(&uid);
        // 静音/归档只放行 @本人（@已在写消息事务内解除归档，能走到这里主要靠静音判断）
        let muted = mute_forever || muted_until.is_some_and(|t| t > now);
        if (muted || archived_at.is_some()) && !mentioned {
            continue;
        }
        // 合并窗口内同一 (用户, 会话) 只响一次；entry 原子占位，并发任务只有一个赢家
        match st.push_last.entry((uid, conv)) {
            Entry::Occupied(e) if e.get().elapsed() < window => continue,
            Entry::Occupied(mut e) => {
                e.insert(Instant::now());
            }
            Entry::Vacant(e) => {
                e.insert(Instant::now());
            }
        }
        let title = if unread > 1 {
            format!("{name} ({unread}条)")
        } else {
            name.clone()
        };
        let body = if preview {
            format!(
                "{}{}: {}",
                if mentioned { "[有人@我] " } else { "" },
                sender_display,
                excerpt(msg)
            )
        } else if mentioned {
            "有人@了你".to_string()
        } else {
            "你收到新消息".to_string()
        };
        for topic in &topics {
            let p = NtfyPayload {
                topic,
                title: &title,
                message: &body,
                priority: if mentioned { 4 } else { 3 },
                tags: &["speech_balloon"],
                click: public_url(&s),
                sequence_id: Some(&seq_id),
            };
            if let Err(e) = send(&s, &p).await {
                tracing::warn!(user = %uid, error = %e, "ntfy push failed");
            }
        }
    }
    Ok(())
}

// ---------- 已读触发：清掉手机上的通知 ----------

/// 已读推进时调用：这个会话推过通知就去 ntfy 把该用户每台手机上的那条清掉，并重置合并窗口
/// （读完后的下一条消息立即提醒）。没推过（无记录）不发请求。
pub fn clear(st: &SharedState, uid: Uuid, conv: Uuid) {
    if st.push_last.remove(&(uid, conv)).is_none() {
        return;
    }
    let st = st.clone();
    tokio::spawn(async move {
        if let Err(e) = clear_devices(&st, uid, conv).await {
            tracing::warn!(user = %uid, conv = %conv, error = ?e, "push clear failed");
        }
    });
}

async fn clear_devices(st: &SharedState, uid: Uuid, conv: Uuid) -> AppResult<()> {
    let s = settings::load(&st.pool).await?;
    if s.push_ntfy_base.is_empty() {
        return Ok(());
    }
    let topics =
        sqlx::query_scalar::<_, String>("SELECT topic FROM push_devices WHERE user_id = $1")
            .bind(uid)
            .fetch_all(&st.pool)
            .await?;
    for topic in topics {
        let url = format!("{}/{}/{}/clear", s.push_ntfy_base, topic, conv);
        if let Err(e) = deliver(&s, client().put(url)).await {
            tracing::warn!(user = %uid, error = %e, "ntfy clear failed");
        }
    }
    Ok(())
}
