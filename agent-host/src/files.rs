// Agent A05：调用附件工作目录。
//
// 每次调用一个临时目录 <data_dir>/inv-<id>/：
//   in/   服务端附件经能力 URL（GET /v1/files/{id}，无需鉴权）下载到这里
//   out/  Agent 产出文件（CHAT_AGENT_OUTBOX 指到这里）
// 调用结束 WorkDir drop 整树删除。
// 产出文件按文件名排序、最多 10 个，逐个用 Invocation 凭据 POST 到
// /v1/agent-tools/files；上传完成后才把 attachment_id 列表放进 invocation_finish。

use crate::runtime::LocalFile;
use serde_json::Value;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// invocation_start.attachments 的单项（服务端给出的元数据）
#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    pub id: Uuid,
    pub name: String,
    pub mime: String,
}

/// 解析 invocation_start.attachments：坏条目跳过，不拖垮整批。
pub fn parse_attachments(v: &Value) -> Vec<AttachmentMeta> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some(AttachmentMeta {
                        id: Uuid::parse_str(e["id"].as_str()?).ok()?,
                        name: e["name"].as_str()?.to_string(),
                        mime: e["mime"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 调用工作目录；Drop 时整树删除（成功/失败/取消/abort 同一条退路）。
pub struct WorkDir(pub PathBuf);

impl WorkDir {
    /// 建 <data_dir>/inv-<id>/{in,out}，返回 (句柄, in, out)。
    /// 建目录失败仍可跑调用：in/out 指向不存在的路径，下载/收集自然落空。
    pub fn create(data_dir: &Path, inv_id: Uuid) -> (Self, PathBuf, PathBuf) {
        let root = data_dir.join(format!("inv-{inv_id}"));
        let in_dir = root.join("in");
        let out_dir = root.join("out");
        if let Err(e) =
            std::fs::create_dir_all(&in_dir).and_then(|_| std::fs::create_dir_all(&out_dir))
        {
            tracing::warn!(error = %e, dir = %root.display(), "调用工作目录创建失败");
        }
        (Self(root), in_dir, out_dir)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            // 目录本就不存在/已删不算错
            if self.0.exists() {
                tracing::warn!(error = %e, dir = %self.0.display(), "调用工作目录删除失败");
            }
        }
    }
}

/// 附件名剥成 basename（剥掉 `../`、`sub/x`、盘符前缀）；剥空退回附件 id。
fn safe_name(name: &str, id: Uuid) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim_start_matches(':')
        .trim();
    if base.is_empty() || base == "." || base == ".." {
        id.to_string()
    } else {
        base.to_string()
    }
}

/// 按附件清单逐个 GET 能力 URL 写进 in/；非 2xx / IO 失败的条目跳过。
/// 返回给运行时的 LocalFile：path 指向 in/<basename>。
pub async fn download_inputs(
    http: &reqwest::Client,
    base: &str,
    in_dir: &Path,
    metas: &[AttachmentMeta],
) -> Vec<LocalFile> {
    let mut out = Vec::with_capacity(metas.len());
    for m in metas {
        let name = safe_name(&m.name, m.id);
        let url = format!("{base}/v1/files/{}", m.id);
        let bytes = match http.get(&url).send().await {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, att = %m.id, "附件下载读体失败，跳过");
                    continue;
                }
            },
            Ok(r) => {
                tracing::warn!(status = %r.status(), att = %m.id, "附件下载被拒，跳过");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, att = %m.id, "附件下载失败，跳过");
                continue;
            }
        };
        let path = in_dir.join(&name);
        if let Err(e) = tokio::fs::write(&path, &bytes).await {
            tracing::warn!(error = %e, att = %m.id, "附件落盘失败，跳过");
            continue;
        }
        out.push(LocalFile {
            path,
            name,
            mime: m.mime.clone(),
        });
    }
    out
}

/// 收 out/ 下的普通文件：按文件名排序，最多 10 个。目录不存在/读不出 → 空。
pub async fn collect_outbox(dir: &Path) -> Vec<PathBuf> {
    let mut names: Vec<(String, PathBuf)> = Vec::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        let ok = e.file_type().await.map(|t| t.is_file()).unwrap_or(false);
        if ok {
            if let Some(n) = e.file_name().to_str().map(str::to_string) {
                names.push((n, e.path()));
            }
        }
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    names.truncate(10);
    names.into_iter().map(|(_, p)| p).collect()
}

/// 把 out/ 的产出逐个上传：POST {base}/v1/agent-tools/files（Invocation 凭据）。
/// 返回 (attachment_id 列表, 因 413 跳过的数量)；其他失败记日志跳过。
/// 无凭据直接返回空——上传必须要调用期凭据。
pub async fn upload_outbox(
    http: &reqwest::Client,
    base: &str,
    out_dir: &Path,
    tools_token: Option<&str>,
) -> (Vec<String>, usize) {
    let Some(token) = tools_token else {
        return (Vec::new(), 0);
    };
    let files = collect_outbox(out_dir).await;
    let mut ids = Vec::new();
    let mut skipped = 0usize;
    for path in files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let data = match tokio::fs::read(&path).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, file = %name, "产出文件读失败，跳过");
                continue;
            }
        };
        let mime = mime_guess::from_path(&path).first_or_octet_stream();
        let part = match reqwest::multipart::Part::bytes(data)
            .file_name(name.clone())
            .mime_str(mime.as_ref())
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, file = %name, "产出文件 MIME 非法，跳过");
                continue;
            }
        };
        let resp = http
            .post(format!("{base}/v1/agent-tools/files"))
            .header("authorization", format!("Invocation {token}"))
            .multipart(reqwest::multipart::Form::new().part("file", part))
            .send()
            .await;
        match resp {
            Ok(r) if r.status() == reqwest::StatusCode::PAYLOAD_TOO_LARGE => {
                skipped += 1;
            }
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(v) => {
                    if let Some(id) = v["attachment_id"].as_str() {
                        ids.push(id.to_string());
                    }
                }
                Err(e) => tracing::warn!(error = %e, file = %name, "产出上传响应解析失败"),
            },
            Ok(r) => {
                tracing::warn!(status = %r.status(), file = %name, "产出上传被拒，跳过");
            }
            Err(e) => {
                tracing::warn!(error = %e, file = %name, "产出上传失败，跳过");
            }
        }
    }
    (ids, skipped)
}
