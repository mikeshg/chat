use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::time::Duration;

pub async fn connect(url: &str) -> PgPool {
    let mut attempt = 0u32;
    loop {
        match PgPoolOptions::new().max_connections(16).connect(url).await {
            Ok(pool) => return pool,
            Err(e) => {
                attempt += 1;
                if attempt > 30 {
                    panic!("cannot connect to database after {attempt} attempts: {e}");
                }
                tracing::warn!(attempt, error = %e, "db connect failed, retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
