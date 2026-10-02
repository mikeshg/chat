use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::model::{AuthResponse, UserPublic, UserRow};
use crate::state::SharedState;
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::http::HeaderMap;
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use uuid::Uuid;

pub const ACCESS_TTL_SECS: i64 = 15 * 60;
pub const REFRESH_TTL_DAYS: i64 = 30;
#[derive(Serialize, Deserialize)]
struct Claims {
    sub: String,
    iat: usize,
    exp: usize,
    /// 签发时的 users.session_version；禁用/改密 bump 后旧 token 失效。
    /// default 兼容升级前签发的旧 token（等同 session_version=0）。
    #[serde(default)]
    sv: i64,
    /// 登录会话 id（一次登录 = 一台设备）；旧 token 无此 claim，不校验 session。
    /// None 时不序列化，使签出的 token 与升级前形状一致。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sid: Option<Uuid>,
    /// 凭据种类。access token 无此字段；旧版签发过 kind="invocation" 的调用凭据，
    /// 任何带 kind 的 token 一律不得通过普通用户鉴权。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
}

/// 校验通过的 access token 承载的身份
pub struct AccessGrant {
    pub uid: Uuid,
    pub session_version: i64,
    /// access token 绑定的登录会话；None = 升级前签发的老 token
    pub sid: Option<Uuid>,
}

/// Argon2 每次几十毫秒纯 CPU：放 blocking 池，不占 async worker。
pub async fn hash_password(pw: &str) -> AppResult<String> {
    let pw = pw.to_string();
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(pw.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(AppError::internal)
    })
    .await
    .map_err(AppError::internal)?
}

pub async fn verify_password(hash: &str, pw: &str) -> bool {
    let (hash, pw) = (hash.to_string(), pw.to_string());
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .ok()
            .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

pub fn mint_access(
    user_id: Uuid,
    session_version: i64,
    sid: Option<Uuid>,
    secret: &str,
) -> AppResult<(String, i64)> {
    let now = Utc::now().timestamp();
    let claims = Claims {
        sub: user_id.to_string(),
        iat: now as usize,
        exp: (now + ACCESS_TTL_SECS) as usize,
        sv: session_version,
        sid,
        kind: None,
    };
    let token = encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(AppError::internal)?;
    Ok((token, ACCESS_TTL_SECS))
}

pub fn verify_access(token: &str, secret: &str) -> AppResult<AccessGrant> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::new(Algorithm::HS256),
    )
    .map_err(|_| AppError::unauthorized("invalid or expired access token"))?;
    // 旧版调用凭据（kind="invocation"）签名也合法，必须在这里挡住
    if data.claims.kind.is_some() {
        return Err(AppError::unauthorized("invalid or expired access token"));
    }
    let uid = Uuid::parse_str(&data.claims.sub)
        .map_err(|_| AppError::unauthorized("bad token subject"))?;
    Ok(AccessGrant {
        uid,
        session_version: data.claims.sv,
        sid: data.claims.sid,
    })
}

/// 生成不透明刷新令牌：(给客户端的原始值, 落库的 sha256 hex)
pub fn mint_refresh() -> (String, String) {
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    let raw = hex::encode(buf);
    let hash = hex::encode(Sha256::digest(raw.as_bytes()));
    (raw, hash)
}

pub fn refresh_hash(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

/// 从 Authorization: Bearer 头解析当前用户身份（uid + 会话版本）
pub fn bearer_grant(headers: &HeaderMap, cfg: &Config) -> AppResult<AccessGrant> {
    let value = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::unauthorized("missing authorization header"))?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| AppError::unauthorized("expected bearer token"))?;
    verify_access(token, &cfg.jwt_secret)
}

/// 通过鉴权的请求上下文：用户行 + access token 绑定的登录会话 id（老 token 为 None）
pub struct AuthCtx {
    pub user: UserRow,
    pub sid: Option<Uuid>,
}

/// WS 连接级复核（升级前、登记后、每帧、宿主 ping 共用这一条 SQL）：
/// 账号未禁用、凭据版本未变、sid 对应的登录会话未撤销；sid 为 None 的老连接不查会话。
pub async fn grant_valid(
    pool: &sqlx::PgPool,
    uid: Uuid,
    session_version: i64,
    sid: Option<Uuid>,
) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT disabled_at IS NULL AND session_version = $2
            AND ($3::uuid IS NULL OR EXISTS(
                 SELECT 1 FROM sessions s
                  WHERE s.id = $3 AND s.user_id = users.id AND s.revoked_at IS NULL))
           FROM users WHERE id = $1",
    )
    .bind(uid)
    .bind(session_version)
    .bind(sid)
    .fetch_one(pool)
    .await
    .unwrap_or(false)
}

/// 解析 token 并取出用户行；账号被禁用、会话版本落后（改密/禁用后签发的旧 token）
/// 或所属登录会话已撤销（sid 存在时一次查询带出）都立即拒绝。
/// sid 为 None 的老 token 不做 session 校验（向后兼容，由 session_version 兜底）。
pub async fn require_auth(st: &SharedState, headers: &HeaderMap) -> AppResult<AuthCtx> {
    let grant = bearer_grant(headers, &st.cfg)?;
    let row = sqlx::query(
        "SELECT u.*, (s.user_id = u.id AND s.revoked_at IS NULL) AS session_ok
           FROM users u LEFT JOIN sessions s ON s.id = $2
          WHERE u.id = $1",
    )
    .bind(grant.uid)
    .bind(grant.sid)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::unauthorized("user not found"))?;
    let session_ok: Option<bool> = sqlx::Row::try_get(&row, "session_ok")?;
    let user = UserRow::from_row(&row)?;
    if user.disabled_at.is_some() {
        return Err(AppError::forbidden("账号已被禁用"));
    }
    if user.session_version as i64 != grant.session_version {
        return Err(AppError::unauthorized(
            "session expired, please login again",
        ));
    }
    if grant.sid.is_some() && session_ok != Some(true) {
        return Err(AppError::unauthorized(
            "session expired, please login again",
        ));
    }
    Ok(AuthCtx {
        user,
        sid: grant.sid,
    })
}

/// 解析 token 并取出用户行；账号被禁用或会话版本落后（改密/禁用后签发的旧 token）立即拒绝
pub async fn require_user(st: &SharedState, headers: &HeaderMap) -> AppResult<UserRow> {
    Ok(require_auth(st, headers).await?.user)
}

/// 管理员门禁：先过 require_user，再校验账号角色
pub async fn require_admin(st: &SharedState, headers: &HeaderMap) -> AppResult<UserRow> {
    let user = require_user(st, headers).await?;
    if user.role != "admin" {
        return Err(AppError::forbidden("需要管理员权限"));
    }
    Ok(user)
}

/// 新建一条登录会话（一次登录 = 一台设备），返回 session id。
pub async fn create_session(
    executor: impl sqlx::PgExecutor<'_>,
    user_id: Uuid,
    device_label: &str,
    user_agent: &str,
) -> AppResult<Uuid> {
    let sid = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO sessions (user_id, device_label, user_agent) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(user_id)
    .bind(device_label)
    .bind(user_agent)
    .fetch_one(executor)
    .await?;
    Ok(sid)
}

/// 签发 access + refresh 令牌并落库（access 绑定当前 session_version 与登录会话 sid）。
/// 调用方须在同一事务内持有该用户行锁（或新插入行），从鉴权直到签发提交。
pub async fn issue_tokens(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cfg: &Config,
    user: &UserRow,
    session_id: Uuid,
) -> AppResult<AuthResponse> {
    let (access, expires_in) = mint_access(
        user.id,
        user.session_version as i64,
        Some(session_id),
        &cfg.jwt_secret,
    )?;
    let (raw, hash) = mint_refresh();
    sqlx::query(
        "INSERT INTO refresh_tokens (user_id, token_hash, session_id, expires_at)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(user.id)
    .bind(hash)
    .bind(session_id)
    .bind(Utc::now() + Duration::days(REFRESH_TTL_DAYS))
    .execute(&mut **tx)
    .await?;
    Ok(AuthResponse {
        access_token: access,
        refresh_token: raw,
        access_expires_in: expires_in,
        user: UserPublic::from(user),
    })
}
