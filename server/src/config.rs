/// 撤回附件默认保留期：上传者取回/重发窗口（秒）。单一 TTL 同时约束
/// GET /files/{id}/recalled、reuse_attachment_id 重发校验与后台清扫。
pub const DEFAULT_RECALL_ATTACHMENT_TTL_SECS: u64 = 86400;
/// 撤回附件默认清扫周期（秒）。
pub const DEFAULT_RECALL_SWEEP_INTERVAL_SECS: u64 = 3600;

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub jwt_secret: String,
    pub bind: String,
    pub port: u16,
    pub app_dist: Option<String>,
    pub files_dir: String,
    pub upload_max_bytes: usize,
    /// 服务端挂在反向代理后面：限流按 X-Forwarded-For 最右一跳认客户端 IP。
    /// 不在代理后面时千万别开，否则谁都能伪造这个头绕过限流。
    pub trust_proxy: bool,
    /// 撤回附件保留期（秒，默认 86400=1 天）：上传者取回/重发的窗口，
    /// 过期后后台清扫删行并按引用计数删物理文件。所有判定用数据库 now()。
    pub recall_attachment_ttl_secs: u64,
    /// 撤回附件清扫周期（秒，默认 3600=每小时）
    pub recall_sweep_interval_secs: u64,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://chat:chat@localhost:15432/chat".into()),
            jwt_secret: jwt_secret_from_env(),
            bind: std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0".into()),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(28080),
            app_dist: std::env::var("APP_DIST").ok().filter(|s| !s.is_empty()),
            files_dir: std::env::var("FILES_DIR").unwrap_or_else(|_| "./data/files".into()),
            upload_max_bytes: std::env::var("UPLOAD_MAX_MB")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(25)
                * 1024
                * 1024,
            trust_proxy: matches!(
                std::env::var("TRUST_PROXY").as_deref(),
                Ok("1") | Ok("true")
            ),
            recall_attachment_ttl_secs: env_secs(
                "RECALL_ATTACHMENT_TTL_SECS",
                DEFAULT_RECALL_ATTACHMENT_TTL_SECS,
            ),
            recall_sweep_interval_secs: env_secs(
                "RECALL_SWEEP_INTERVAL_SECS",
                DEFAULT_RECALL_SWEEP_INTERVAL_SECS,
            ),
        }
    }
}

fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// 凭据签名密钥。缺省或太短就拒绝启动：公开的默认值等于谁都能伪造任意用户的 token。
fn jwt_secret_from_env() -> String {
    let secret = std::env::var("JWT_SECRET").unwrap_or_default();
    if secret.trim().len() < 32 || secret == "dev-secret-change-me" {
        panic!("JWT_SECRET 未设置或少于 32 个字符。用 `openssl rand -hex 32` 生成，写进 .env 或环境变量");
    }
    secret
}

#[cfg(test)]
mod tests {
    #[test]
    fn weak_jwt_secret_refuses_to_start() {
        for bad in ["", "short", "dev-secret-change-me"] {
            std::env::set_var("JWT_SECRET", bad);
            assert!(std::panic::catch_unwind(super::jwt_secret_from_env).is_err(), "{bad:?}");
        }
        std::env::set_var("JWT_SECRET", "x".repeat(32));
        assert_eq!(super::jwt_secret_from_env().len(), 32);
    }
}
