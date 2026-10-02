#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub jwt_secret: String,
    pub bind: String,
    pub port: u16,
    pub app_dist: Option<String>,
    pub files_dir: String,
    pub upload_max_bytes: usize,
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
        }
    }
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
