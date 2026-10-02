// Agent A05：调用附件工作目录。
//
// 每次调用一个临时目录 <data_dir>/inv-<id>/：
//   in/   服务端附件经能力 URL（GET /v1/files/{id}，无需鉴权）下载到 in/<附件 id>/
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

/// 附件名清洗成单个普通文件名：剥掉路径分隔与开头盘符前缀、冒号（含 NTFS
/// 备用数据流写法）、Windows 非法字符与控制字符、结尾的点和空格；保留设备名
/// （CON/NUL/COM1 等，含 `nul.txt` 这类带扩展名的）改掉；剥空退回附件 id。
/// A19：服务端不清洗 multipart 文件名，转发附件名原样来自群友，必须防住
/// `C:foo.txt` 这类「有前缀无根」路径——join 会整个替换，写出 in/ 外。
fn safe_name(name: &str, id: Uuid) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let base = strip_drive_prefix(base);
    let mut out: String = base.chars().filter(|c| !is_bad_char(*c)).collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    if out.is_empty() {
        return id.to_string();
    }
    if is_reserved_name(&out) {
        out.insert(0, '_');
    }
    out
}

/// 剥掉开头的 `X:` 盘符前缀（盘符相对路径 `C:foo` 的 `C:`）。
fn strip_drive_prefix(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        &s[2..]
    } else {
        s
    }
}

/// Windows 文件名非法字符 + 控制字符；冒号含 NTFS 备用数据流写法。
fn is_bad_char(c: char) -> bool {
    c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\')
}

/// Windows 保留设备名：看首个 `.` 前的词干（大小写不敏感）。
fn is_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end();
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// 按附件清单逐个 GET 能力 URL 写进 in/<附件 id>/（每附件一个子目录，同名附件
/// 不互相覆盖）；非 2xx / IO 失败 / 清洗后仍不安全的条目跳过。
/// 返回给运行时的 LocalFile：path 指向 in/<id>/<清洗后文件名>，name 是清洗后的
/// 可读名（ACP resource_link.name 用它）。
pub async fn download_inputs(
    http: &reqwest::Client,
    base: &str,
    in_dir: &Path,
    metas: &[AttachmentMeta],
) -> Vec<LocalFile> {
    let mut out = Vec::with_capacity(metas.len());
    for m in metas {
        let name = safe_name(&m.name, m.id);
        let dir = in_dir.join(m.id.to_string());
        let path = dir.join(&name);
        // 兜底断言：落盘路径必须是 in/<附件 id>/ 的直接孩子
        if path.parent() != Some(dir.as_path()) {
            tracing::warn!(att = %m.id, name = %m.name, "附件名清洗后仍不安全，跳过");
            continue;
        }
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
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            tracing::warn!(error = %e, att = %m.id, "附件目录创建失败，跳过");
            continue;
        }
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

/// 断网 / 5xx 时重试多久。服务端给掉线宿主留 10 分钟，调用期凭据在这段时间里一直有效。
const UPLOAD_RETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(9 * 60);

/// 上传产出文件的结果
#[derive(Debug, Default, PartialEq)]
pub struct Uploaded {
    /// 成功的 attachment_id（按上传顺序）
    pub ids: Vec<String>,
    /// 因配额 / 大小被拒（413）
    pub skipped: usize,
    /// 重试到期仍失败、或被服务端拒绝；不能悄悄丢，要在回复里说明
    pub failed: usize,
}

/// 把 out/ 的产出逐个上传：POST {base}/v1/agent-tools/files（Invocation 凭据）。
/// 断网、5xx、429 退避重试（总共 UPLOAD_RETRY_WINDOW）；413 计 skipped；
/// 401 说明调用已结束，剩下的全部计 failed。无凭据直接返回空——上传必须要调用期凭据。
pub async fn upload_outbox(
    http: &reqwest::Client,
    base: &str,
    out_dir: &Path,
    tools_token: Option<&str>,
) -> Uploaded {
    let mut res = Uploaded::default();
    let Some(token) = tools_token else {
        return res;
    };
    let files = collect_outbox(out_dir).await;
    let total = files.len();
    let deadline = std::time::Instant::now() + UPLOAD_RETRY_WINDOW;
    let mut wait = std::time::Duration::from_secs(1);
    for (i, path) in files.into_iter().enumerate() {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let data = match tokio::fs::read(&path).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, file = %name, "产出文件读失败");
                res.failed += 1;
                continue;
            }
        };
        let mime = mime_guess::from_path(&path).first_or_octet_stream();
        loop {
            let part = match reqwest::multipart::Part::bytes(data.clone())
                .file_name(name.clone())
                .mime_str(mime.as_ref())
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, file = %name, "产出文件 MIME 非法");
                    res.failed += 1;
                    break;
                }
            };
            let resp = http
                .post(format!("{base}/v1/agent-tools/files"))
                .header("authorization", format!("Invocation {token}"))
                .multipart(reqwest::multipart::Form::new().part("file", part))
                .send()
                .await;
            let transient = match resp {
                Ok(r) if r.status() == reqwest::StatusCode::PAYLOAD_TOO_LARGE => {
                    res.skipped += 1;
                    break;
                }
                Ok(r) if r.status().is_success() => {
                    match r.json::<Value>().await {
                        Ok(v) if v["attachment_id"].is_string() => {
                            res.ids.push(v["attachment_id"].as_str().unwrap().to_string());
                        }
                        other => {
                            tracing::warn!(?other, file = %name, "产出上传响应解析失败");
                            res.failed += 1;
                        }
                    }
                    break;
                }
                Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => {
                    tracing::warn!(file = %name, "调用已结束，剩余产出无法上传");
                    res.failed += total - i;
                    return res;
                }
                Ok(r) if r.status().is_server_error()
                    || r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS =>
                {
                    format!("HTTP {}", r.status())
                }
                Ok(r) => {
                    tracing::warn!(status = %r.status(), file = %name, "产出上传被拒");
                    res.failed += 1;
                    break;
                }
                Err(e) => e.to_string(),
            };
            if std::time::Instant::now() + wait > deadline {
                tracing::warn!(error = %transient, file = %name, "产出上传重试到期");
                res.failed += 1;
                break;
            }
            tracing::info!(error = %transient, file = %name, ?wait, "产出上传失败，稍后重试");
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(std::time::Duration::from_secs(30));
        }
    }
    res
}
