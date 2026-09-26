use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    PayloadTooLarge(String),
    /// 上游服务（如 ntfy）不可达/拒绝——推送测试端点用
    BadGateway(String),
    /// 等宿主回包超时（登记表列表 20 秒）
    GatewayTimeout(String),
    Internal(String),
}

impl AppError {
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::BadRequest(m.into())
    }
    pub fn unauthorized(m: impl Into<String>) -> Self {
        Self::Unauthorized(m.into())
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::Forbidden(m.into())
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::NotFound(m.into())
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::Conflict(m.into())
    }
    pub fn payload_too_large(m: impl Into<String>) -> Self {
        Self::PayloadTooLarge(m.into())
    }
    pub fn internal(m: impl std::fmt::Display) -> Self {
        Self::Internal(m.to_string())
    }
    pub fn bad_gateway(m: impl Into<String>) -> Self {
        Self::BadGateway(m.into())
    }
    pub fn gateway_timeout(m: impl Into<String>) -> Self {
        Self::GatewayTimeout(m.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Self::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m),
            Self::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            Self::NotFound(m) => (StatusCode::NOT_FOUND, m),
            Self::Conflict(m) => (StatusCode::CONFLICT, m),
            Self::PayloadTooLarge(m) => (StatusCode::PAYLOAD_TOO_LARGE, m),
            Self::BadGateway(m) => (StatusCode::BAD_GATEWAY, m),
            Self::GatewayTimeout(m) => (StatusCode::GATEWAY_TIMEOUT, m),
            Self::Internal(m) => {
                tracing::error!(error = %m, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        (code, Json(json!({ "error": msg }))).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => Self::not_found("not found"),
            other => Self::internal(other),
        }
    }
}

pub fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}
