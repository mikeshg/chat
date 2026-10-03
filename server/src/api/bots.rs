//! Webhook bot：主人名下的特殊账号（users.bot_owner_id）。不能登录、搜不到、收不了好友申请。
//! 入站：POST /v1/hooks 带 Authorization: Bearer <令牌>，以 bot 身份发消息（默认发到和主人的私聊）。
//! 一个 bot 可以有多个令牌，分给不同程序、单独删除。
//! 出站：bot 在私聊里收到消息、或在群里被 @ 时，服务端把消息 POST 到 webhook_url，
//! 用 secret 签名；响应 JSON 里的 content 作为 bot 的回复发回同一会话。

use super::*;
use sha2::{Digest, Sha256};

/// 每人最多这么多 bot（防手滑刷号）
const MAX_BOTS: i64 = 20;
/// 每个 bot 最多这么多入站令牌
const MAX_TOKENS: i64 = 20;
const WEBHOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// 回调响应只读这么多字节；超出当作无回复
const REPLY_MAX_BYTES: usize = 64 * 1024;

#[derive(Serialize, sqlx::FromRow)]
pub(super) struct BotView {
    id: Uuid,
    username: String,
    display_name: String,
    webhook_url: String,
    /// 只有主人看得到。回调请求用它签名
    secret: String,
    /// 入站令牌 [{id, name, token, created_at}]，按创建时间排
    tokens: sqlx::types::Json<Value>,
    created_at: DateTime<Utc>,
}

const BOT_SELECT: &str = "SELECT u.id, u.username::text AS username, u.display_name,
        b.webhook_url, b.secret, b.created_at,
        COALESCE((SELECT json_agg(json_build_object(
                    'id', t.id, 'name', t.name, 'token', t.token, 'created_at', t.created_at)
                  ORDER BY t.created_at)
                    FROM bot_tokens t WHERE t.bot_id = b.user_id), '[]') AS tokens
   FROM bots b JOIN users u ON u.id = b.user_id";

#[derive(Deserialize)]
pub(super) struct CreateBotReq {
    username: String,
    display_name: Option<String>,
    #[serde(default)]
    webhook_url: String,
}

#[derive(Deserialize)]
pub(super) struct PatchBotReq {
    display_name: Option<String>,
    webhook_url: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CreateTokenReq {
    name: String,
}

#[derive(Deserialize)]
pub(super) struct HookReq {
    content: String,
    /// 缺省 = bot 和主人的私聊
    conversation_id: Option<Uuid>,
    /// 幂等键（1–80 字符）：同一 bot 同一会话重发同一个值，返回第一次那条消息、不重复发
    client_msg_id: Option<String>,
}

fn new_token() -> String {
    let mut buf = [0u8; 32];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut buf);
    hex::encode(buf)
}

/// 空串 = 不回调；否则 http/https、≤500 字符
fn normalize_webhook(raw: &str) -> AppResult<String> {
    let url = raw.trim();
    let ok = url.is_empty()
        || (url.len() <= 500
            && (url.starts_with("http://") || url.starts_with("https://"))
            && !url.chars().any(char::is_whitespace));
    if !ok {
        return Err(AppError::bad_request("webhook_url 必须是 http/https 地址"));
    }
    Ok(url.to_string())
}

fn normalize_display(raw: &str) -> AppResult<String> {
    let name = raw.trim();
    if name.is_empty() || name.len() > 50 {
        return Err(AppError::bad_request("display_name must be 1-50 chars"));
    }
    Ok(name.to_string())
}

async fn load_bot(st: &SharedState, owner: Uuid, bot: Uuid) -> AppResult<BotView> {
    sqlx::query_as::<_, BotView>(&format!(
        "{BOT_SELECT} WHERE b.user_id = $1 AND b.owner_id = $2"
    ))
    .bind(bot)
    .bind(owner)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::not_found("bot not found"))
}

/// GET /bots：本人的 bot
pub(super) async fn list_bots(
    State(st): State<SharedState>,
    headers: HeaderMap,
) -> AppResult<Json<Vec<BotView>>> {
    let uid = uid_of(&headers, &st).await?;
    let rows = sqlx::query_as::<_, BotView>(&format!(
        "{BOT_SELECT} WHERE b.owner_id = $1 ORDER BY b.created_at"
    ))
    .bind(uid)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows))
}

/// POST /bots：建账号 + 和主人成为好友 + 建私聊
pub(super) async fn create_bot(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<CreateBotReq>,
) -> AppResult<Json<BotView>> {
    let uid = uid_of(&headers, &st).await?;
    let username = req.username.trim();
    if !account::valid_username(username) {
        return Err(AppError::bad_request(
            "username must be 3-20 chars of [a-zA-Z0-9_]",
        ));
    }
    let display = normalize_display(req.display_name.as_deref().unwrap_or(username))?;
    let webhook = normalize_webhook(&req.webhook_url)?;

    let mut tx = st.pool.begin().await?;
    // 主人行锁：并发创建也数得准上限
    sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    let n = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM bots WHERE owner_id = $1")
        .bind(uid)
        .fetch_one(&mut *tx)
        .await?;
    if n >= MAX_BOTS {
        return Err(AppError::bad_request("bot 数量已达上限"));
    }
    // 密码哈希填不可解析的占位：verify_password 恒 false，bot 无法登录
    let bot = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO users (username, display_name, password_hash, bot_owner_id)
         VALUES ($1, $2, '!', $3) RETURNING id",
    )
    .bind(username)
    .bind(&display)
    .bind(uid)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            AppError::conflict("username taken")
        } else {
            e.into()
        }
    })?;
    let (lo, hi) = domain::canonical_pair(uid, bot);
    sqlx::query("INSERT INTO friendships (user_a, user_b) VALUES ($1, $2)")
        .bind(lo)
        .bind(hi)
        .execute(&mut *tx)
        .await?;
    domain::ensure_dm(&mut tx, uid, bot).await?;
    sqlx::query(
        "INSERT INTO bots (user_id, owner_id, webhook_url, secret) VALUES ($1, $2, $3, $4)",
    )
    .bind(bot)
    .bind(uid)
    .bind(&webhook)
    .bind(new_token())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    notify(&st, &[uid], "friends");
    notify(&st, &[uid], "conversations");
    Ok(Json(load_bot(&st, uid, bot).await?))
}

/// PATCH /bots/{id}：改昵称 / 回调地址
pub(super) async fn patch_bot(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(bot): Path<Uuid>,
    Json(req): Json<PatchBotReq>,
) -> AppResult<Json<BotView>> {
    let uid = uid_of(&headers, &st).await?;
    load_bot(&st, uid, bot).await?;
    if let Some(name) = &req.display_name {
        sqlx::query("UPDATE users SET display_name = $2 WHERE id = $1")
            .bind(bot)
            .bind(normalize_display(name)?)
            .execute(&st.pool)
            .await?;
    }
    if let Some(url) = &req.webhook_url {
        sqlx::query("UPDATE bots SET webhook_url = $2 WHERE user_id = $1")
            .bind(bot)
            .bind(normalize_webhook(url)?)
            .execute(&st.pool)
            .await?;
    }
    notify(&st, &[uid], "friends");
    Ok(Json(load_bot(&st, uid, bot).await?))
}

/// POST /bots/{id}/secret：换签名密钥
pub(super) async fn reset_bot_secret(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(bot): Path<Uuid>,
) -> AppResult<Json<BotView>> {
    let uid = uid_of(&headers, &st).await?;
    load_bot(&st, uid, bot).await?;
    sqlx::query("UPDATE bots SET secret = $2 WHERE user_id = $1")
        .bind(bot)
        .bind(new_token())
        .execute(&st.pool)
        .await?;
    Ok(Json(load_bot(&st, uid, bot).await?))
}

/// POST /bots/{id}/tokens：加一个入站令牌
pub(super) async fn create_bot_token(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(bot): Path<Uuid>,
    Json(req): Json<CreateTokenReq>,
) -> AppResult<Json<BotView>> {
    let uid = uid_of(&headers, &st).await?;
    load_bot(&st, uid, bot).await?;
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 50 {
        return Err(AppError::bad_request("令牌名称 1-50 个字"));
    }
    let mut tx = st.pool.begin().await?;
    // bot 行锁：并发添加也数得准上限
    sqlx::query("SELECT 1 FROM bots WHERE user_id = $1 FOR UPDATE")
        .bind(bot)
        .execute(&mut *tx)
        .await?;
    let n = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM bot_tokens WHERE bot_id = $1")
        .bind(bot)
        .fetch_one(&mut *tx)
        .await?;
    if n >= MAX_TOKENS {
        return Err(AppError::bad_request("令牌数量已达上限"));
    }
    sqlx::query("INSERT INTO bot_tokens (bot_id, name, token) VALUES ($1, $2, $3)")
        .bind(bot)
        .bind(name)
        .bind(new_token())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(load_bot(&st, uid, bot).await?))
}

/// DELETE /bots/{id}/tokens/{token_id}：删令牌，立即失效
pub(super) async fn delete_bot_token(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((bot, token_id)): Path<(Uuid, Uuid)>,
) -> AppResult<Json<BotView>> {
    let uid = uid_of(&headers, &st).await?;
    load_bot(&st, uid, bot).await?;
    let n = sqlx::query("DELETE FROM bot_tokens WHERE id = $1 AND bot_id = $2")
        .bind(token_id)
        .bind(bot)
        .execute(&st.pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("token not found"));
    }
    Ok(Json(load_bot(&st, uid, bot).await?))
}

/// DELETE /bots/{id}：账号禁用、退出所有群、和主人解除好友（私聊变只读）。
/// 发过的消息保留（messages.sender_id 不级联），所以账号行不删。
pub(super) async fn delete_bot(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path(bot): Path<Uuid>,
) -> AppResult<StatusCode> {
    let uid = uid_of(&headers, &st).await?;
    load_bot(&st, uid, bot).await?;
    let mut tx = st.pool.begin().await?;
    let groups = sqlx::query_scalar::<_, Uuid>(
        "DELETE FROM memberships m USING conversations c
          WHERE m.conversation_id = c.id AND m.user_id = $1 AND c.type = 'group'
          RETURNING c.id",
    )
    .bind(bot)
    .fetch_all(&mut *tx)
    .await?;
    let (lo, hi) = domain::canonical_pair(uid, bot);
    sqlx::query("DELETE FROM friendships WHERE user_a = $1 AND user_b = $2")
        .bind(lo)
        .bind(hi)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
        .bind(bot)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM bots WHERE user_id = $1")
        .bind(bot)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut affected = vec![uid];
    for g in groups {
        affected.extend(domain::member_ids(&st.pool, g).await?);
    }
    notify(&st, &[uid], "friends");
    notify(&st, &affected, "conversations");
    Ok(StatusCode::NO_CONTENT)
}

/// POST /hooks：Authorization: Bearer <bot 令牌>，以 bot 身份发一条文字消息。不走用户鉴权。
pub(super) async fn post_hook(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<HookReq>,
) -> AppResult<Json<Value>> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| AppError::unauthorized("expected bearer token"))?;
    let (bot, owner) = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT b.user_id, b.owner_id FROM bot_tokens t JOIN bots b ON b.user_id = t.bot_id
          WHERE t.token = $1",
    )
    .bind(token)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::unauthorized("invalid bot token"))?;
    let conv = match req.conversation_id {
        Some(c) => c,
        None => {
            let (lo, hi) = domain::canonical_pair(bot, owner);
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM conversations WHERE dm_key = $1")
                .bind(format!("{lo}:{hi}"))
                .fetch_optional(&st.pool)
                .await?
                .ok_or_else(|| AppError::not_found("conversation not found"))?
        }
    };
    let posted = ws::post_and_broadcast(
        &st,
        domain::PostMessage {
            client_msg_id: req.client_msg_id.as_deref(),
            ..domain::PostMessage::text(conv, bot, &req.content)
        },
    )
    .await
    .map_err(post_err)?;
    Ok(Json(json!({
        "id": posted.message.id,
        "conversation_id": conv,
        "seq": posted.message.seq,
    })))
}

/// 新消息落库后调用：收件的 bot 有回调地址就异步 POST 过去，不拖慢广播。
/// 私聊里 bot 是对方就回调；群里只有被 @ 的 bot 回调。bot 发的消息不触发任何 bot（防互相 @ 死循环）。
pub(crate) fn dispatch(st: &SharedState, conv: Uuid, msg: &MessageView) {
    let st = st.clone();
    let msg = msg.clone();
    tokio::spawn(async move {
        let targets = match sqlx::query_as::<_, (Uuid, String, String, String)>(
            "SELECT b.user_id, b.webhook_url, b.secret, c.type
               FROM bots b
               JOIN memberships m ON m.user_id = b.user_id AND m.conversation_id = $1
               JOIN conversations c ON c.id = $1
               JOIN users s ON s.id = $2
              WHERE b.webhook_url <> '' AND b.user_id <> $2 AND s.bot_owner_id IS NULL
                AND (c.type = 'dm' OR b.user_id = ANY($3))",
        )
        .bind(conv)
        .bind(msg.sender_id)
        .bind(&msg.mentions)
        .fetch_all(&st.pool)
        .await
        {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "bot dispatch lookup failed");
                return;
            }
        };
        for (bot, url, secret, kind) in targets {
            let st = st.clone();
            let msg = msg.clone();
            tokio::spawn(async move {
                match call_webhook(&url, &secret, bot, conv, &kind, &msg).await {
                    Ok(Some(reply)) => {
                        let r = ws::post_and_broadcast(
                            &st,
                            domain::PostMessage::text(conv, bot, &reply),
                        )
                        .await;
                        if let Err(e) = r {
                            tracing::warn!(bot = %bot, error = ?e, "bot reply rejected");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(bot = %bot, url = %url, error = %e, "bot webhook failed")
                    }
                }
            });
        }
    });
}

fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(WEBHOOK_TIMEOUT)
            // bot 常在局域网里，不走系统代理
            .no_proxy()
            .build()
            .expect("build reqwest client")
    })
}

/// POST 消息给 bot；返回响应 JSON 里非空的 content
async fn call_webhook(
    url: &str,
    secret: &str,
    bot: Uuid,
    conv: Uuid,
    kind: &str,
    msg: &MessageView,
) -> Result<Option<String>, String> {
    let body = serde_json::to_vec(&json!({
        "bot_id": bot,
        "conversation_id": conv,
        "conversation_type": kind,
        "message": msg,
    }))
    .map_err(|e| e.to_string())?;
    let sig = hex::encode(hmac_sha256(secret.as_bytes(), &body));
    let mut resp = client()
        .post(url)
        .header("content-type", "application/json")
        .header("x-chat-signature", format!("sha256={sig}"))
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status()));
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        buf.extend_from_slice(&chunk);
        if buf.len() > REPLY_MAX_BYTES {
            return Err("reply too large".into());
        }
    }
    Ok(serde_json::from_slice::<Value>(&buf)
        .ok()
        .and_then(|v| v["content"].as_str().map(str::to_owned))
        .filter(|s| !s.trim().is_empty()))
}

/// HMAC-SHA256（RFC 2104）。只此一处用，不为它加依赖。
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hmac_matches_rfc4231_case_2() {
        assert_eq!(
            hex::encode(super::hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
