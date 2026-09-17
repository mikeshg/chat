use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::model::{AuthResponse, UserPublic};
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
use sqlx::PgPool;
use uuid::Uuid;

pub const ACCESS_TTL_SECS: i64 = 15 * 60;
pub const REFRESH_TTL_DAYS: i64 = 30;

#[derive(Serialize, Deserialize)]
struct Claims {
    sub: String,
    iat: usize,
    exp: usize,
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

pub fn mint_access(user_id: Uuid, secret: &str) -> AppResult<(String, i64)> {
    let now = Utc::now().timestamp();
    let claims = Claims {
        sub: user_id.to_string(),
        iat: now as usize,
        exp: (now + ACCESS_TTL_SECS) as usize,
    };
    let token = encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(AppError::internal)?;
    Ok((token, ACCESS_TTL_SECS))
}

pub fn verify_access(token: &str, secret: &str) -> AppResult<Uuid> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::new(Algorithm::HS256),
    )
    .map_err(|_| AppError::unauthorized("invalid or expired access token"))?;
    Uuid::parse_str(&data.claims.sub).map_err(|_| AppError::unauthorized("bad token subject"))
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

/// 从 Authorization: Bearer 头解析当前用户 id
pub fn bearer_uid(headers: &HeaderMap, cfg: &Config) -> AppResult<Uuid> {
    let value = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::unauthorized("missing authorization header"))?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| AppError::unauthorized("expected bearer token"))?;
    verify_access(token, &cfg.jwt_secret)
}

/// 签发 access + refresh 令牌并落库
pub async fn issue_tokens(
    pool: &PgPool,
    cfg: &Config,
    user: &UserPublic,
    device_label: Option<&str>,
) -> AppResult<AuthResponse> {
    let (access, expires_in) = mint_access(user.id, &cfg.jwt_secret)?;
    let (raw, hash) = mint_refresh();
    sqlx::query(
        "INSERT INTO refresh_tokens (user_id, token_hash, device_label, expires_at)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(user.id)
    .bind(hash)
    .bind(device_label)
    .bind(Utc::now() + Duration::days(REFRESH_TTL_DAYS))
    .execute(pool)
    .await?;
    Ok(AuthResponse {
        access_token: access,
        refresh_token: raw,
        access_expires_in: expires_in,
        user: user.clone(),
    })
}
