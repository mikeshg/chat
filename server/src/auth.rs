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
}

/// 校验通过的 access token 承载的身份
pub struct AccessGrant {
    pub uid: Uuid,
    pub session_version: i64,
}

pub fn hash_password(pw: &str) -> AppResult<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(AppError::internal)
}

pub fn verify_password(hash: &str, pw: &str) -> bool {
    PasswordHash::new(hash)
        .ok()
        .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

pub fn mint_access(user_id: Uuid, session_version: i64, secret: &str) -> AppResult<(String, i64)> {
    let now = Utc::now().timestamp();
    let claims = Claims {
        sub: user_id.to_string(),
        iat: now as usize,
        exp: (now + ACCESS_TTL_SECS) as usize,
        sv: session_version,
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
    let uid = Uuid::parse_str(&data.claims.sub)
        .map_err(|_| AppError::unauthorized("bad token subject"))?;
    Ok(AccessGrant {
        uid,
        session_version: data.claims.sv,
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

/// 解析 token 并取出用户行；账号被禁用或会话版本落后（改密/禁用后签发的旧 token）立即拒绝
pub async fn require_user(st: &SharedState, headers: &HeaderMap) -> AppResult<UserRow> {
    let grant = bearer_grant(headers, &st.cfg)?;
    let user = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1")
        .bind(grant.uid)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::unauthorized("user not found"))?;
    if user.disabled_at.is_some() {
        return Err(AppError::forbidden("账号已被禁用"));
    }
    if user.session_version as i64 != grant.session_version {
        return Err(AppError::unauthorized(
            "session expired, please login again",
        ));
    }
    Ok(user)
}

/// 管理员门禁：先过 require_user，再校验账号角色
pub async fn require_admin(st: &SharedState, headers: &HeaderMap) -> AppResult<UserRow> {
    let user = require_user(st, headers).await?;
    if user.role != "admin" {
        return Err(AppError::forbidden("需要管理员权限"));
    }
    Ok(user)
}

/// 签发 access + refresh 令牌并落库（access 绑定当前 session_version）。
/// 调用方须在同一事务内持有该用户行锁（或新插入行），从鉴权直到签发提交。
pub async fn issue_tokens(
    executor: impl sqlx::PgExecutor<'_>,
    cfg: &Config,
    user: &UserRow,
    device_label: Option<&str>,
) -> AppResult<AuthResponse> {
    let (access, expires_in) = mint_access(user.id, user.session_version as i64, &cfg.jwt_secret)?;
    let (raw, hash) = mint_refresh();
    sqlx::query(
        "INSERT INTO refresh_tokens (user_id, token_hash, device_label, expires_at)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(user.id)
    .bind(hash)
    .bind(device_label)
    .bind(Utc::now() + Duration::days(REFRESH_TTL_DAYS))
    .execute(executor)
    .await?;
    Ok(AuthResponse {
        access_token: access,
        refresh_token: raw,
        access_expires_in: expires_in,
        user: UserPublic::from(user),
    })
}
