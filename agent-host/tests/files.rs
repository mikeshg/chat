// Agent A05：调用附件工作目录的单元测试。
// collect_outbox / WorkDir / parse_attachments / download_inputs / upload_outbox。

use agent_host::files::{self, AttachmentMeta};
use axum::{
    extract::{Multipart, Path},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::Mutex;
use uuid::Uuid;

fn metas(names: &[&str]) -> Vec<AttachmentMeta> {
    names
        .iter()
        .map(|n| AttachmentMeta {
            id: Uuid::new_v4(),
            name: n.to_string(),
            mime: "text/plain".into(),
        })
        .collect()
}

/// parse_attachments：合法条目进、非法条目跳过，坏条目不拖垮整批
#[test]
fn parse_attachments_skips_bad_entries() {
    let id = Uuid::new_v4();
    let v = json!([
        {"id": id.to_string(), "name": "a.txt", "mime": "text/plain"},
        {"id": "not-a-uuid", "name": "b.txt", "mime": "text/plain"},
        {"id": id.to_string()},
        "junk",
        {"id": Uuid::new_v4().to_string(), "name": "c.png", "mime": "image/png"},
    ]);
    let got = files::parse_attachments(&v);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].name, "a.txt");
    assert_eq!(got[1].mime, "image/png");
    // 字段缺失/非数组 → 空
    assert!(files::parse_attachments(&json!({})).is_empty());
    assert!(files::parse_attachments(&json!(null)).is_empty());
}

/// WorkDir drop 即删整棵树（含 abort 路径的兜底语义）
#[test]
fn workdir_drop_removes_tree() {
    let root = std::env::temp_dir().join(format!("a05-wd-{}", Uuid::new_v4().simple()));
    let in_dir = root.join("in");
    std::fs::create_dir_all(&in_dir).unwrap();
    std::fs::write(in_dir.join("f.txt"), b"x").unwrap();
    {
        let _wd = files::WorkDir(root.clone());
    }
    assert!(!root.exists(), "drop 后目录应被删除");
}

/// collect_outbox：只收普通文件、按文件名排序、上限 10
#[tokio::test]
async fn collect_outbox_sorted_capped_files_only() {
    let dir = std::env::temp_dir().join(format!("a05-out-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    for i in (0..12).rev() {
        std::fs::write(dir.join(format!("f{i:02}.txt")), b"b").unwrap();
    }
    std::fs::create_dir_all(dir.join("subdir")).unwrap(); // 目录不收
    let got = files::collect_outbox(&dir).await;
    assert_eq!(got.len(), 10);
    for (i, p) in got.iter().enumerate() {
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            format!("f{i:02}.txt")
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 无 outbox 目录/空目录 → 空
#[tokio::test]
async fn collect_outbox_empty_or_missing() {
    let dir = std::env::temp_dir().join(format!("a05-none-{}", Uuid::new_v4().simple()));
    assert!(files::collect_outbox(&dir).await.is_empty());
    std::fs::create_dir_all(&dir).unwrap();
    assert!(files::collect_outbox(&dir).await.is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- 本地 stub 服务端：GET /v1/files/{id} 回固定字节；POST /v1/agent-tools/files 记认证头 ----

struct Up {
    auth: Option<String>,
    #[allow(dead_code)]
    name: String,
}

struct Stub {
    uploads: Mutex<Vec<Up>>,
}

async fn dl(Path(id): Path<String>) -> impl IntoResponse {
    // nil UUID 模拟「附件已失效」：回 404 让下载侧走跳过路径
    if id == Uuid::nil().to_string() {
        return StatusCode::NOT_FOUND.into_response();
    }
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain")],
        format!("bytes-of-{id}"),
    )
        .into_response()
}

async fn up(
    axum::extract::State(st): axum::extract::State<std::sync::Arc<Stub>>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> axum::response::Response {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if auth.as_deref() != Some("Invocation tok-1") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    while let Some(f) = mp.next_field().await.unwrap() {
        if f.name() == Some("file") {
            let name = f.file_name().unwrap_or("?").to_string();
            let data = f.bytes().await.unwrap();
            st.uploads.lock().unwrap().push(Up {
                auth,
                name: name.clone(),
            });
            if name.starts_with("big") {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({"error": "quota_exceeded"})),
                )
                    .into_response();
            }
            return Json(json!({"attachment_id": format!("att-{name}-{}", data.len())}))
                .into_response();
        }
    }
    StatusCode::BAD_REQUEST.into_response()
}

async fn stub() -> (String, std::sync::Arc<Stub>) {
    let st = std::sync::Arc::new(Stub {
        uploads: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/v1/files/{id}", get(dl))
        .route("/v1/agent-tools/files", post(up))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), st)
}

/// download_inputs：逐附件 GET 能力 URL 写进 in/；404/失败条目跳过
#[tokio::test]
async fn download_inputs_writes_in_dir_and_skips_failures() {
    let (base, _st) = stub().await;
    let root = std::env::temp_dir().join(format!("a05-dl-{}", Uuid::new_v4().simple()));
    let in_dir = root.join("in");
    tokio::fs::create_dir_all(&in_dir).await.unwrap();
    let mut ms = metas(&["a.txt", "sub/../escape.txt", "missing.txt"]);
    ms[2].id = Uuid::nil(); // stub 对 nil id 回 404
    let http = reqwest::Client::new();
    let got = files::download_inputs(&http, &base, &in_dir, &ms).await;
    assert_eq!(got.len(), 2, "404 条目应被跳过: {got:?}");
    assert_eq!(got[0].name, "a.txt");
    // 路径穿越被剥掉：文件名只取 basename
    assert_eq!(got[1].name, "escape.txt");
    assert!(got[1].path.starts_with(&in_dir));
    let data = std::fs::read_to_string(&got[0].path).unwrap();
    assert!(data.starts_with("bytes-of-"), "{data}");

    // 空清单 → 空
    let got = files::download_inputs(&http, &base, &in_dir, &[]).await;
    assert!(got.is_empty());
    std::fs::remove_dir_all(&root).unwrap();
}

/// upload_outbox：按序上传、413 计 skipped 继续、其余失败跳过；无 token 直接跳过
#[tokio::test]
async fn upload_outbox_order_skip_and_auth() {
    let (base, st) = stub().await;
    let dir = std::env::temp_dir().join(format!("a05-up-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), b"aa").unwrap();
    std::fs::write(dir.join("big.bin"), vec![0u8; 8]).unwrap();
    std::fs::write(dir.join("c.txt"), b"ccc").unwrap();
    let http = reqwest::Client::new();

    let (ids, skipped) = files::upload_outbox(&http, &base, &dir, Some("tok-1")).await;
    // 排序 a < big < c；big 413 跳过，两个成功
    assert_eq!(ids, vec!["att-a.txt-2", "att-c.txt-3"]);
    assert_eq!(skipped, 1);
    let ups = st.uploads.lock().unwrap();
    assert_eq!(ups.len(), 3);
    assert!(ups
        .iter()
        .all(|u| u.auth.as_deref() == Some("Invocation tok-1")));
    drop(ups);

    // 无 token：全部不可传
    let (ids, skipped) = files::upload_outbox(&http, &base, &dir, None).await;
    assert!(ids.is_empty());
    assert_eq!(skipped, 0);

    // 凭据错误 → 401（非 413）：记日志跳过，不计 skipped
    let (ids, skipped) = files::upload_outbox(&http, &base, &dir, Some("bad-tok")).await;
    assert!(ids.is_empty());
    assert_eq!(skipped, 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// download_inputs 目标目录写不进（被文件占位）→ 该附件跳过
#[tokio::test]
async fn download_inputs_io_error_skipped() {
    let (base, _st) = stub().await;
    let root = std::env::temp_dir().join(format!("a05-io-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).unwrap();
    let blocker = root.join("in");
    std::fs::write(&blocker, b"not-a-dir").unwrap(); // in/ 是文件 → 写入必失败
    let http = reqwest::Client::new();
    let got = files::download_inputs(&http, &base, &blocker, &metas(&["x.txt"])).await;
    assert!(got.is_empty());
    std::fs::remove_dir_all(&root).unwrap();
}

/// Value 里 entry 引用：保持 meta.id 稳定往返
#[test]
fn parse_attachments_keeps_ids() {
    let id = Uuid::new_v4();
    let v = json!([{"id": id.to_string(), "name": "x", "mime": "m", "size": 9}]);
    let got = files::parse_attachments(&v);
    assert_eq!(got[0].id, id);
    let _ = Value::Null;
}

// ===== Agent A19 =====

/// 附件名清洗：盘符相对路径 / 路径穿越 / 保留设备名 / ADS 写法都不能逃出 in/<id>/
#[tokio::test]
async fn download_inputs_sanitizes_hostile_names() {
    let (base, _st) = stub().await;
    let root = std::env::temp_dir().join(format!("a19-dl-{}", Uuid::new_v4().simple()));
    let in_dir = root.join("in");
    tokio::fs::create_dir_all(&in_dir).await.unwrap();
    let ms = metas(&[
        "C:foo.txt", // 盘符相对路径：join 会被「有前缀无根」整段替换，逃出 in/
        "..\\..\\x", // 路径穿越
        "a/b/c.txt", // 正斜杠路径
        "nul.txt",   // Windows 保留设备名（含扩展名也算）
        "a:stream",  // NTFS 备用数据流写法
        "",          // 空名 → 退回附件 id
    ]);
    let http = reqwest::Client::new();
    let got = files::download_inputs(&http, &base, &in_dir, &ms).await;
    assert_eq!(got.len(), ms.len(), "全都应落盘: {got:?}");
    for (m, f) in ms.iter().zip(&got) {
        // 落在 in/<附件 id>/ 下且是该目录的直接孩子
        assert_eq!(
            f.path.parent().unwrap(),
            in_dir.join(m.id.to_string()),
            "{} 不在自己的目录里",
            f.path.display()
        );
        assert!(f.path.starts_with(&in_dir), "{:?} 逃出 in/", f.path);
        // 清洗后的名字是单个普通文件名：无分隔、无冒号、无非法字符
        assert!(!f.name.is_empty());
        assert!(
            !f.name
                .contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*']),
            "{}",
            f.name
        );
        let data = std::fs::read(&f.path).unwrap();
        assert_eq!(data, format!("bytes-of-{}", m.id).into_bytes());
    }
    assert_eq!(got[0].name, "foo.txt"); // C:foo.txt → 剥盘符
    assert_eq!(got[1].name, "x"); // ..\..\x → basename
    assert_eq!(got[2].name, "c.txt"); // a/b/c.txt → basename
    assert_ne!(got[3].name, "nul.txt"); // 保留设备名必须改掉
    assert_eq!(got[5].name, ms[5].id.to_string()); // 空名 → id
    std::fs::remove_dir_all(&root).unwrap();
}

/// 同名附件各落各的目录，内容互不覆盖
#[tokio::test]
async fn download_inputs_same_names_do_not_clobber() {
    let (base, _st) = stub().await;
    let root = std::env::temp_dir().join(format!("a19-dup-{}", Uuid::new_v4().simple()));
    let in_dir = root.join("in");
    tokio::fs::create_dir_all(&in_dir).await.unwrap();
    let ms = metas(&["report.txt", "report.txt"]);
    let http = reqwest::Client::new();
    let got = files::download_inputs(&http, &base, &in_dir, &ms).await;
    assert_eq!(got.len(), 2);
    assert_ne!(got[0].path, got[1].path, "同名附件必须落不同路径");
    // Agent 看到的仍是可读原名
    assert_eq!(got[0].name, "report.txt");
    assert_eq!(got[1].name, "report.txt");
    for (m, f) in ms.iter().zip(&got) {
        let data = std::fs::read(&f.path).unwrap();
        assert_eq!(data, format!("bytes-of-{}", m.id).into_bytes());
    }
    std::fs::remove_dir_all(&root).unwrap();
}
