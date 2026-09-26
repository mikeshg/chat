// ACP 官方登记表：拉取、24 小时缓存、本机可用性、安装、解析成启动命令。
//
// 索引默认 https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json
// 顶层 { version, agents: [...] }。分发方式可以并存：npx / uvx / binary。
// 安装落在数据目录，不写回服务端（每台机器路径不一样）。

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const STAGE_DOWNLOAD: &str = "下载";
pub const STAGE_VERIFY: &str = "校验";
pub const STAGE_EXTRACT: &str = "解压";
pub const STAGE_DONE: &str = "完成";
pub const STAGE_FAILED: &str = "失败";

const TTL: Duration = Duration::from_secs(24 * 60 * 60);
const NOT_INSTALLED: &str = "registry_not_installed";

/// 列表里的一条。reason 空串表示没有需要展示的原因。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ListedEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub icon: String,
    pub kinds: Vec<String>,
    pub status: String,
    pub reason: String,
}

#[derive(Debug)]
pub struct ListOutcome {
    pub entries: Vec<ListedEntry>,
    pub error: Option<String>,
}

/// 解析后的本机启动方式。command 是可执行文件路径（uvx 时是命令名）。
#[derive(Debug, Clone)]
pub struct Launch {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

struct PkgDist {
    package: String,
    args: Vec<String>,
}

struct BinDist {
    archive: String,
    sha256: Option<String>,
    cmd: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

struct AgentRec {
    id: String,
    name: String,
    version: String,
    description: String,
    icon: String,
    npx: Option<PkgDist>,
    uvx: Option<PkgDist>,
    binary: BTreeMap<String, BinDist>,
}

pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("registry-cache.json")
}

/// 当前机器在登记表 binary 里用的平台键。
pub fn current_platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        _ => "unknown",
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let dig = Sha256::digest(bytes);
    let mut s = String::with_capacity(dig.len() * 2);
    for b in dig {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 读缓存或拉取。now 由调用方给，测试可以拨快时钟。
/// 缓存未过期就不再请求；拉取失败且有旧缓存（哪怕过期）就用旧的；两边都没有才 Err。
pub async fn load_document_at(
    cache_file: &Path,
    url: &str,
    now: SystemTime,
) -> Result<Value, String> {
    let cached = read_cache(cache_file);
    if let Some((at, body)) = &cached {
        if is_fresh(*at, now) {
            return Ok(body.clone());
        }
    }
    match fetch_registry(url).await {
        Ok(body) => {
            if let Err(e) = write_cache(cache_file, now, &body) {
                tracing::warn!("登记表缓存写入失败: {e}");
            }
            Ok(body)
        }
        Err(e) => match cached {
            Some((_, body)) => Ok(body),
            None => Err(format!("无法获取登记表，且没有本地缓存：{e}")),
        },
    }
}

pub async fn list_local(data_dir: &Path, url: &str) -> ListOutcome {
    match load_document_at(&cache_path(data_dir), url, SystemTime::now()).await {
        Ok(doc) => ListOutcome {
            entries: classify(data_dir, &doc),
            error: None,
        },
        Err(e) => ListOutcome {
            entries: Vec::new(),
            error: Some(e),
        },
    }
}

/// 主人确认后的安装。进度回调的 stage 是：下载 / 校验 / 解压 / 完成 / 失败。
pub async fn install<F>(data_dir: &Path, url: &str, entry_id: &str, version: &str, mut progress: F)
where
    F: FnMut(&str, Option<&str>) + Send,
{
    if let Err(e) = safe_component(entry_id).and(safe_component(version)) {
        progress(STAGE_FAILED, Some(&e));
        return;
    }
    let doc = match load_document_at(&cache_path(data_dir), url, SystemTime::now()).await {
        Ok(d) => d,
        Err(e) => {
            progress(STAGE_FAILED, Some(&e));
            return;
        }
    };
    let Some(agent) = parse_agents(&doc).into_iter().find(|a| a.id == entry_id) else {
        progress(STAGE_FAILED, Some("登记表里没有这个条目"));
        return;
    };
    if agent.version != version {
        progress(STAGE_FAILED, Some("登记表里没有这个版本"));
        return;
    }
    match choose_install(data_dir, &agent) {
        Choice::Binary(bin) => install_binary(data_dir, &agent, &bin, &mut progress).await,
        Choice::Npx(pkg) => install_npx(data_dir, &agent, &pkg, &mut progress).await,
        Choice::Already => progress(STAGE_DONE, None),
        Choice::Unsupported(reason) => progress(STAGE_FAILED, Some(&reason)),
    }
}

/// 调用前把登记表条目解成本机命令。没装、或这个版本不在本机 → `registry_not_installed`。
pub async fn resolve_launch(
    data_dir: &Path,
    url: &str,
    id: &str,
    version: &str,
) -> Result<Launch, String> {
    if safe_component(id).is_err() || safe_component(version).is_err() {
        return Err(NOT_INSTALLED.into());
    }
    if let Some(launch) = read_install(&bin_dir(data_dir, id, version)) {
        return Ok(launch);
    }
    if let Some(launch) = read_install(&npm_prefix(data_dir, id, version)) {
        return Ok(launch);
    }
    let doc = load_document_at(&cache_path(data_dir), url, SystemTime::now())
        .await
        .map_err(|_| NOT_INSTALLED.to_string())?;
    let Some(agent) = parse_agents(&doc).into_iter().find(|a| a.id == id) else {
        return Err(NOT_INSTALLED.into());
    };
    if agent.version != version {
        return Err(NOT_INSTALLED.into());
    }
    if let Some(bin) = agent.binary.get(current_platform()) {
        let path = bin_dir(data_dir, id, version).join(&bin.cmd);
        if path.is_file() {
            return Ok(Launch {
                command: path.display().to_string(),
                args: bin.args.clone(),
                env: bin.env.clone(),
            });
        }
    }
    if let Some(pkg) = &agent.npx {
        if let Some(command) = npx_bin(&npm_prefix(data_dir, id, version), &pkg.package) {
            return Ok(Launch {
                command,
                args: pkg.args.clone(),
                env: Vec::new(),
            });
        }
    }
    if let Some(pkg) = &agent.uvx {
        if tool_present("uvx") {
            let args = if pkg.args.is_empty() {
                vec![format!("{}@{version}", pkg.package)]
            } else {
                pkg.args.clone()
            };
            return Ok(Launch {
                command: "uvx".into(),
                args,
                env: Vec::new(),
            });
        }
    }
    Err(NOT_INSTALLED.into())
}

fn is_fresh(fetched_at: u64, now: SystemTime) -> bool {
    let now_s = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    now_s.saturating_sub(fetched_at) < TTL.as_secs()
}

fn read_cache(path: &Path) -> Option<(u64, Value)> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let at = v.get("fetched_at")?.as_u64()?;
    let body = v.get("body")?.clone();
    body.is_object().then_some((at, body))
}

fn write_cache(path: &Path, now: SystemTime, body: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let text = serde_json::to_string(&json!({"fetched_at": secs, "body": body}))
        .map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

async fn fetch_registry(url: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json().await.map_err(|e| format!("不是 JSON：{e}"))
}

fn classify(data_dir: &Path, doc: &Value) -> Vec<ListedEntry> {
    parse_agents(doc)
        .into_iter()
        .map(|a| {
            let (status, reason) = status_of(data_dir, &a);
            let mut kinds = Vec::new();
            if a.npx.is_some() {
                kinds.push("npx".into());
            }
            if a.uvx.is_some() {
                kinds.push("uvx".into());
            }
            if !a.binary.is_empty() {
                kinds.push("binary".into());
            }
            ListedEntry {
                id: a.id,
                name: a.name,
                version: a.version,
                description: a.description,
                icon: a.icon,
                kinds,
                status: status.into(),
                reason,
            }
        })
        .collect()
}

struct Cand {
    rank: u8,
    reason: String,
}

fn status_of(data_dir: &Path, agent: &AgentRec) -> (&'static str, String) {
    let mut cands = Vec::new();
    if let Some(pkg) = &agent.npx {
        cands.push(npx_cand(data_dir, agent, pkg));
    }
    if agent.uvx.is_some() {
        cands.push(if tool_present("uvx") {
            Cand {
                rank: 2,
                reason: String::new(),
            }
        } else {
            Cand {
                rank: 0,
                reason: "需要先安装 uv".into(),
            }
        });
    }
    if !agent.binary.is_empty() {
        cands.push(binary_cand(data_dir, agent));
    }
    if cands.is_empty() {
        return ("unsupported", "没有可用的分发方式".into());
    }
    let best = cands.iter().map(|c| c.rank).max().unwrap_or(0);
    if best == 0 {
        let mut reasons = Vec::new();
        for c in &cands {
            if c.rank == 0 && !c.reason.is_empty() && !reasons.contains(&c.reason) {
                reasons.push(c.reason.clone());
            }
        }
        return ("unsupported", reasons.join("；"));
    }
    let status = if best == 2 { "ready" } else { "installable" };
    (status, String::new())
}

fn npx_cand(data_dir: &Path, agent: &AgentRec, pkg: &PkgDist) -> Cand {
    if !tool_present("node") || !tool_present("npm") {
        return Cand {
            rank: 0,
            reason: "需要先安装 Node.js".into(),
        };
    }
    if npx_bin(
        &npm_prefix(data_dir, &agent.id, &agent.version),
        &pkg.package,
    )
    .is_some()
    {
        Cand {
            rank: 2,
            reason: String::new(),
        }
    } else {
        Cand {
            rank: 1,
            reason: String::new(),
        }
    }
}

fn binary_cand(data_dir: &Path, agent: &AgentRec) -> Cand {
    let Some(bin) = agent.binary.get(current_platform()) else {
        return Cand {
            rank: 0,
            reason: "当前系统没有对应的安装包".into(),
        };
    };
    if bin.cmd.is_empty() || !path_is_relative_safe(Path::new(&bin.cmd)) {
        return Cand {
            rank: 0,
            reason: "当前系统没有对应的安装包".into(),
        };
    }
    let path = bin_dir(data_dir, &agent.id, &agent.version).join(&bin.cmd);
    if path.is_file() {
        Cand {
            rank: 2,
            reason: String::new(),
        }
    } else {
        Cand {
            rank: 1,
            reason: String::new(),
        }
    }
}

enum Choice {
    Binary(BinDist),
    Npx(PkgDist),
    Already,
    Unsupported(String),
}

fn choose_install(data_dir: &Path, agent: &AgentRec) -> Choice {
    if let Some(bin) = agent.binary.get(current_platform()) {
        let path = bin_dir(data_dir, &agent.id, &agent.version).join(&bin.cmd);
        if !path.is_file() && path_is_relative_safe(Path::new(&bin.cmd)) && !bin.cmd.is_empty() {
            return Choice::Binary(clone_bin(bin));
        }
    }
    if let Some(pkg) = &agent.npx {
        if tool_present("node")
            && tool_present("npm")
            && npx_bin(
                &npm_prefix(data_dir, &agent.id, &agent.version),
                &pkg.package,
            )
            .is_none()
        {
            return Choice::Npx(PkgDist {
                package: pkg.package.clone(),
                args: pkg.args.clone(),
            });
        }
    }
    let (status, reason) = status_of(data_dir, agent);
    if status == "ready" {
        Choice::Already
    } else {
        Choice::Unsupported(if reason.is_empty() {
            "无法安装".into()
        } else {
            reason
        })
    }
}

fn clone_bin(bin: &BinDist) -> BinDist {
    BinDist {
        archive: bin.archive.clone(),
        sha256: bin.sha256.clone(),
        cmd: bin.cmd.clone(),
        args: bin.args.clone(),
        env: bin.env.clone(),
    }
}

async fn install_binary<F>(data_dir: &Path, agent: &AgentRec, bin: &BinDist, progress: &mut F)
where
    F: FnMut(&str, Option<&str>) + Send,
{
    if bin.archive.is_empty() {
        progress(STAGE_FAILED, Some("没有下载地址"));
        return;
    }
    progress(STAGE_DOWNLOAD, None);
    let bytes = match download(&bin.archive).await {
        Ok(b) => b,
        Err(e) => {
            progress(STAGE_FAILED, Some(&e));
            return;
        }
    };
    if let Some(sum) = bin
        .sha256
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        progress(STAGE_VERIFY, None);
        if sha256_hex(&bytes) != sum.to_ascii_lowercase() {
            progress(STAGE_FAILED, Some("sha256 不符"));
            return;
        }
    }
    progress(STAGE_EXTRACT, None);
    let dest = bin_dir(data_dir, &agent.id, &agent.version);
    let archive = bin.archive.clone();
    let extracted = tokio::task::spawn_blocking(move || extract_archive(&bytes, &archive, &dest))
        .await
        .unwrap_or_else(|e| Err(format!("解压失败：{e}")));
    if let Err(e) = extracted {
        let _ = std::fs::remove_dir_all(bin_dir(data_dir, &agent.id, &agent.version));
        progress(STAGE_FAILED, Some(&e));
        return;
    }
    let cmd_path = bin_dir(data_dir, &agent.id, &agent.version).join(&bin.cmd);
    if !cmd_path.is_file() {
        progress(STAGE_FAILED, Some("压缩包里没有启动命令"));
        return;
    }
    if let Err(e) = write_install(
        &bin_dir(data_dir, &agent.id, &agent.version),
        &bin.cmd,
        &bin.args,
        &bin.env,
        true,
    ) {
        progress(STAGE_FAILED, Some(&e));
        return;
    }
    progress(STAGE_DONE, None);
}

async fn install_npx<F>(data_dir: &Path, agent: &AgentRec, pkg: &PkgDist, progress: &mut F)
where
    F: FnMut(&str, Option<&str>) + Send,
{
    progress(STAGE_DOWNLOAD, None);
    let prefix = npm_prefix(data_dir, &agent.id, &agent.version);
    if let Err(e) = std::fs::create_dir_all(&prefix) {
        progress(STAGE_FAILED, Some(&format!("无法创建安装目录：{e}")));
        return;
    }
    let spec = format!("{}@{}", pkg.package, agent.version);
    let mut cmd = crate::command::prepare(
        "npm",
        &[
            "install".into(),
            "--prefix".into(),
            prefix.display().to_string(),
            spec,
        ],
    );
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .current_dir(&prefix);
    let out = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            progress(STAGE_FAILED, Some(&format!("npm 启动失败：{e}")));
            return;
        }
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: String = err
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let msg = format!("npm install 失败：{tail}");
        progress(STAGE_FAILED, Some(&msg));
        return;
    }
    let Some(bin) = npx_bin(&prefix, &pkg.package) else {
        progress(STAGE_FAILED, Some("安装完成但找不到可执行文件"));
        return;
    };
    if let Err(e) = write_install(&prefix, &bin, &pkg.args, &[], false) {
        progress(STAGE_FAILED, Some(&e));
        return;
    }
    progress(STAGE_DONE, None);
}

async fn download(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("下载失败：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载失败：HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("下载失败：{e}"))?;
    Ok(bytes.to_vec())
}

fn extract_archive(bytes: &[u8], url: &str, dest: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        extract_tar_gz(bytes, dest)
    } else if path.ends_with(".zip") {
        extract_zip(bytes, dest)
    } else {
        Err("不认识的压缩包格式（只支持 zip 和 tar.gz）".into())
    }
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("解压失败：{e}"))?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| format!("解压失败：{e}"))?;
        let Some(rel) = file.enclosed_name().map(|p| p.to_path_buf()) else {
            return Err("压缩包路径不合法".into());
        };
        let out = dest.join(&rel);
        if file.name().ends_with('/') || file.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut file, &mut f).map_err(|e| format!("解压失败：{e}"))?;
    }
    Ok(())
}

fn extract_tar_gz(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let dec = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(dec);
    for entry in archive.entries().map_err(|e| format!("解压失败：{e}"))? {
        let mut entry = entry.map_err(|e| format!("解压失败：{e}"))?;
        let rel = entry.path().map_err(|e| e.to_string())?.into_owned();
        if !path_is_relative_safe(&rel) {
            return Err("压缩包路径不合法".into());
        }
        let out = dest.join(&rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        entry.unpack(&out).map_err(|e| format!("解压失败：{e}"))?;
    }
    Ok(())
}

fn path_is_relative_safe(path: &Path) -> bool {
    use std::path::Component;
    let mut any = false;
    for c in path.components() {
        match c {
            Component::Normal(_) => any = true,
            Component::CurDir => {}
            _ => return false,
        }
    }
    any
}

fn bin_dir(data_dir: &Path, id: &str, version: &str) -> PathBuf {
    data_dir.join("agents").join("bin").join(id).join(version)
}

fn npm_prefix(data_dir: &Path, id: &str, version: &str) -> PathBuf {
    data_dir
        .join("agents")
        .join("npm")
        .join(format!("{id}@{version}"))
}

fn write_install(
    dir: &Path,
    command: &str,
    args: &[String],
    env: &[(String, String)],
    relative: bool,
) -> Result<(), String> {
    let env_map: serde_json::Map<String, Value> =
        env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let text = serde_json::to_string(&json!({
        "command": command,
        "args": args,
        "env": env_map,
        "relative": relative,
    }))
    .map_err(|e| e.to_string())?;
    std::fs::write(dir.join("install.json"), text).map_err(|e| e.to_string())
}

fn read_install(dir: &Path) -> Option<Launch> {
    let text = std::fs::read_to_string(dir.join("install.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let command = v.get("command")?.as_str()?.to_string();
    let relative = v.get("relative").and_then(Value::as_bool).unwrap_or(false);
    let command = if relative {
        dir.join(&command).display().to_string()
    } else {
        command
    };
    if !Path::new(&command).is_file() {
        return None;
    }
    let args = string_list(v.get("args"));
    let env = v
        .get("env")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(Launch { command, args, env })
}

/// npm 装好之后，node_modules/.bin 里对应包的可执行文件。
fn npx_bin(prefix: &Path, package: &str) -> Option<String> {
    let pkg_json = node_modules_pkg(prefix, package).join("package.json");
    let text = std::fs::read_to_string(pkg_json).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let bin_name = bin_name(&v, package)?;
    let bin_dir = prefix.join("node_modules").join(".bin");
    let path = bin_dir.display().to_string();
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| {
        if cfg!(windows) {
            ".COM;.EXE;.BAT;.CMD".into()
        } else {
            String::new()
        }
    });
    crate::command::resolve_in(&bin_name, &path, &pathext).map(|p| p.display().to_string())
}

fn node_modules_pkg(prefix: &Path, package: &str) -> PathBuf {
    let mut p = prefix.join("node_modules");
    for part in package.split('/') {
        if !part.is_empty() {
            p = p.join(part);
        }
    }
    p
}

fn bin_name(pkg: &Value, package: &str) -> Option<String> {
    let unscoped = package.rsplit('/').next().unwrap_or(package).to_string();
    match pkg.get("bin")? {
        Value::String(_) => Some(unscoped),
        Value::Object(map) if map.contains_key(&unscoped) => Some(unscoped),
        Value::Object(map) => map.keys().next().cloned(),
        _ => None,
    }
}

fn tool_present(name: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| {
        if cfg!(windows) {
            ".COM;.EXE;.BAT;.CMD".into()
        } else {
            String::new()
        }
    });
    crate::command::resolve_in(name, &path, &pathext).is_some()
}

fn safe_component(s: &str) -> Result<(), String> {
    if s.is_empty()
        || s.chars()
            .any(|c| c.is_whitespace() || c == '/' || c == '\\' || c == '\0')
        || s.contains("..")
    {
        Err("id 或版本不合法".into())
    } else {
        Ok(())
    }
}

fn parse_agents(doc: &Value) -> Vec<AgentRec> {
    doc.get("agents")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_agent).collect())
        .unwrap_or_default()
}

fn parse_agent(v: &Value) -> Option<AgentRec> {
    let id = json_str(v.get("id"))?;
    if id.is_empty() {
        return None;
    }
    let version = json_str(v.get("version")).unwrap_or_default();
    if version.is_empty() {
        return None;
    }
    let name = json_str(v.get("name"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| id.clone());
    let dist = v.get("distribution");
    Some(AgentRec {
        id,
        name,
        version,
        description: json_str(v.get("description")).unwrap_or_default(),
        icon: json_str(v.get("icon")).unwrap_or_default(),
        npx: dist.and_then(|d| parse_pkg(d.get("npx"))),
        uvx: dist.and_then(|d| parse_pkg(d.get("uvx"))),
        binary: dist
            .and_then(|d| d.get("binary"))
            .map(parse_binary)
            .unwrap_or_default(),
    })
}

fn parse_pkg(v: Option<&Value>) -> Option<PkgDist> {
    let v = v?;
    let package = json_str(v.get("package"))?;
    if package.is_empty() {
        return None;
    }
    Some(PkgDist {
        package,
        args: string_list(v.get("args")),
    })
}

fn parse_binary(v: &Value) -> BTreeMap<String, BinDist> {
    let Some(obj) = v.as_object() else {
        return BTreeMap::new();
    };
    obj.iter()
        .filter_map(|(k, val)| {
            let cmd = json_str(val.get("cmd"))?;
            let archive = json_str(val.get("archive")).unwrap_or_default();
            let sha256 = json_str(val.get("sha256")).filter(|s| !s.is_empty());
            let env = val
                .get("env")
                .and_then(Value::as_object)
                .map(|o| {
                    o.iter()
                        .filter_map(|(ek, ev)| ev.as_str().map(|s| (ek.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            Some((
                k.clone(),
                BinDist {
                    archive,
                    sha256,
                    cmd,
                    args: string_list(val.get("args")),
                    env,
                },
            ))
        })
        .collect()
}

fn json_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
