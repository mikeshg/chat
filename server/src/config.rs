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
            jwt_secret: std::env::var("JWT_SECRET")
                .unwrap_or_else(|_| "dev-secret-change-me".into()),
            bind: std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0".into()),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(8080),
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
