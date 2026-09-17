pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod model;
pub mod state;
pub mod ws;

use axum::Router;
use config::Config;
use state::{AppState, SharedState};
use std::net::SocketAddr;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

/// 构建完整路由（API + 可选静态托管）。测试复用。
pub fn build_router(state: SharedState, cfg: &Config) -> Router {
    let mut app = Router::new()
        .nest("/v1", api::router())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    if let Some(dist) = &cfg.app_dist {
        let index = format!("{dist}/index.html");
        app = app.fallback_service(ServeDir::new(dist).not_found_service(ServeFile::new(index)));
    }
    app
}

pub async fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env();
    let pool = db::connect(&cfg.database_url).await;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations failed");

    let state = AppState::new(pool, cfg.clone());
    let app = build_router(state, &cfg);

    let addr: SocketAddr = format!("{}:{}", cfg.bind, cfg.port).parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind failed");
    tracing::info!(%addr, "chat-server listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .expect("server error");
}
