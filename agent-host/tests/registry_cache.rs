// Agent A07：登记表缓存 24 小时；过期重拉；拉取失败回退旧缓存。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn scratch() -> PathBuf {
    // 并行测试可能拿到同一个时间戳（Windows 时钟 100ns 粒度），加计数保证各用各的目录
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "a07-cache-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn body(name: &str) -> Value {
    json!({
        "version": 1,
        "agents": [{
            "id": "a",
            "name": name,
            "version": "1.0.0",
            "description": "",
            "distribution": {
                "binary": {
                    "linux-x86_64": {"archive": "http://example.invalid/a.zip", "cmd": "a"}
                }
            }
        }]
    })
}

fn write_cache(path: &std::path::Path, at: SystemTime, doc: &Value) {
    let secs = at.duration_since(UNIX_EPOCH).unwrap().as_secs();
    let text = json!({"fetched_at": secs, "body": doc}).to_string();
    std::fs::write(path, text).unwrap();
}

async fn serve(hits: Arc<AtomicUsize>, status: u16, doc: Value) -> String {
    use axum::{routing::get, Router};
    let app = Router::new().route(
        "/registry.json",
        get(move || {
            let hits = hits.clone();
            let doc = doc.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    doc.to_string(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/registry.json")
}

#[tokio::test]
async fn fresh_cache_skips_fetch() {
    let dir = scratch();
    let cache = dir.join("registry-cache.json");
    write_cache(&cache, SystemTime::now(), &body("from-cache"));
    let hits = Arc::new(AtomicUsize::new(0));
    let url = serve(hits.clone(), 200, body("from-net")).await;
    let doc = agent_host::registry::load_document_at(&cache, &url, SystemTime::now())
        .await
        .expect("fresh cache");
    assert_eq!(doc["agents"][0]["name"], "from-cache");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "未过期不应访问网络");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn expired_cache_refetches() {
    let dir = scratch();
    let cache = dir.join("registry-cache.json");
    let old = SystemTime::now() - Duration::from_secs(25 * 60 * 60);
    write_cache(&cache, old, &body("stale"));
    let hits = Arc::new(AtomicUsize::new(0));
    let url = serve(hits.clone(), 200, body("fresh")).await;
    let doc = agent_host::registry::load_document_at(&cache, &url, SystemTime::now())
        .await
        .expect("refetch");
    assert_eq!(doc["agents"][0]["name"], "fresh");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn fetch_failure_falls_back_to_stale_cache() {
    let dir = scratch();
    let cache = dir.join("registry-cache.json");
    let old = SystemTime::now() - Duration::from_secs(48 * 60 * 60);
    write_cache(&cache, old, &body("kept"));
    let hits = Arc::new(AtomicUsize::new(0));
    let url = serve(hits.clone(), 500, body("nope")).await;
    let doc = agent_host::registry::load_document_at(&cache, &url, SystemTime::now())
        .await
        .expect("stale cache should be used");
    assert_eq!(doc["agents"][0]["name"], "kept");
    assert!(hits.load(Ordering::SeqCst) >= 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn fetch_failure_without_cache_is_an_error() {
    let dir = scratch();
    let cache = dir.join("missing.json");
    let hits = Arc::new(AtomicUsize::new(0));
    let url = serve(hits, 500, body("nope")).await;
    let err = agent_host::registry::load_document_at(&cache, &url, SystemTime::now())
        .await
        .expect_err("没有缓存时应失败");
    assert!(err.contains("无法获取登记表"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
