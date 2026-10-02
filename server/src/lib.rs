pub mod api;
pub mod auth;
pub mod backup;
pub mod client_update;
pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod media;
pub mod model;
pub mod push;
pub mod sessions;
pub mod settings;
pub mod state;
pub mod ws;

use axum::Router;
use config::Config;
use state::{AppState, SharedState};
use std::net::SocketAddr;
use axum::http::{header, HeaderValue};
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

/// 构建完整路由（API + 可选静态托管）。测试复用。
pub fn build_router(state: SharedState, cfg: &Config) -> Router {
    // V2 P4-1：维护模式闸。恢复进行中除当前恢复请求外所有 /v1 请求（含 WS 升级）503。
    // 未知的 /v1 路径回 JSON 404（带 CORS）：客户端比服务端新时能说清楚是版本问题。
    // 不设这个会落到下面的静态托管兜底，POST 回一个不带 CORS 的 405，客户端只看到 Failed to fetch。
    let v1 = api::router(cfg)
        .fallback(|| async { error::AppError::not_found("unknown_api") })
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            backup::maintenance_gate,
        ));
    let mut app = Router::new()
        .nest("/v1", v1)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    if let Some(dist) = &cfg.app_dist {
        let index = format!("{dist}/index.html");
        let pages = ServeDir::new(dist).not_found_service(ServeFile::new(index));
        // 网页端的 CSP 与 Tauri 壳一致（tauri.conf.json）。登录凭据存在 localStorage，
        // 一旦有 XSS 就能拿走：CSP 只允许本站脚本，是最后一道闸。
        let headers = tower::ServiceBuilder::new()
            .layer(SetResponseHeaderLayer::if_not_present(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(WEB_CSP),
            ))
            .layer(SetResponseHeaderLayer::if_not_present(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ))
            .layer(SetResponseHeaderLayer::if_not_present(
                header::REFERRER_POLICY,
                HeaderValue::from_static("no-referrer"),
            ));
        app = app.fallback_service(headers.service(pages));
    }
    app
}

/// 和 app/src-tauri/tauri.conf.json 的 csp 保持一致，另加不许被别的站点嵌进 iframe（点击劫持）。
const WEB_CSP: &str = "default-src 'self'; connect-src 'self' http: https: ws: wss:; img-src 'self' data: blob: http: https:; media-src 'self' http: https: blob:; frame-src 'self' http: https:; object-src 'none'; style-src 'self' 'unsafe-inline'; script-src 'self'; base-uri 'self'; frame-ancestors 'self'";

pub async fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env();
    std::fs::create_dir_all(&cfg.files_dir).expect("create files dir failed");
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
