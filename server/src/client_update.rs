//! 客户端在线更新。安装包由 Gitea Actions 打好、挂在 Gitea Release 上；
//! 服务端读仓库的最新 Release 转给客户端，下载也经服务端中转（客户端不一定连得到 Gitea）。
//! 仓库地址由管理员在后台配置（server_settings.update_repo_url），空 = 功能关闭。

use crate::error::{AppError, AppResult};
use crate::settings;
use crate::state::SharedState;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;

/// "http://h:3000/owner/repo" → "http://h:3000/api/v1/repos/owner/repo"；不是仓库地址 → None
pub fn api_base(repo_url: &str) -> Option<String> {
    let (scheme, rest) = repo_url.split_once("://")?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let host = parts.next().filter(|h| !h.is_empty())?;
    let path: Vec<&str> = parts.collect();
    // Gitea 可能挂在子路径下：最后两段是 owner/repo，前面的都是前缀
    let [prefix @ .., owner, repo] = path.as_slice() else {
        return None;
    };
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let prefix: String = prefix.iter().map(|p| format!("/{p}")).collect();
    Some(format!(
        "{scheme}://{host}{prefix}/api/v1/repos/{owner}/{repo}"
    ))
}

#[derive(Deserialize)]
struct GiteaRelease {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    published_at: String,
    #[serde(default)]
    assets: Vec<GiteaAsset>,
}

#[derive(Deserialize)]
struct GiteaAsset {
    name: String,
    size: i64,
    browser_download_url: String,
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Gitea 在内网，不走系统代理
            .no_proxy()
            .build()
            .expect("build reqwest client")
    })
}

async fn latest_release(st: &SharedState) -> AppResult<GiteaRelease> {
    let s = settings::load(&st.pool).await?;
    let api = api_base(&s.update_repo_url).ok_or_else(|| AppError::not_found("update_off"))?;
    let resp = client()
        .get(format!("{api}/releases/latest"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| AppError::bad_gateway(format!("Gitea 不可达：{e}")))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AppError::not_found("no_release"));
    }
    if !resp.status().is_success() {
        return Err(AppError::bad_gateway(format!(
            "Gitea 返回 {}",
            resp.status()
        )));
    }
    resp.json()
        .await
        .map_err(|e| AppError::bad_gateway(format!("Gitea 响应无法解析：{e}")))
}

fn version_of(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

#[derive(Serialize)]
pub struct AssetView {
    pub name: String,
    pub size: i64,
}

#[derive(Serialize)]
pub struct ClientLatest {
    pub version: String,
    pub notes: String,
    pub published_at: String,
    /// 下载地址：/v1/client/download/{name}
    pub assets: Vec<AssetView>,
}

/// GET /client/latest：公共端点。没配置仓库 / 还没有 Release → 404
pub async fn latest(State(st): State<SharedState>) -> AppResult<Json<ClientLatest>> {
    let r = latest_release(&st).await?;
    Ok(Json(ClientLatest {
        version: version_of(&r.tag_name).to_string(),
        notes: r.body,
        published_at: r.published_at,
        assets: r
            .assets
            .into_iter()
            .map(|a| AssetView {
                name: a.name,
                size: a.size,
            })
            .collect(),
    }))
}

/// 本服务对外地址：优先后台配置的 public_url，否则按请求的 Host 拼
fn origin(public_url: &str, headers: &HeaderMap) -> String {
    if !public_url.is_empty() {
        return public_url.to_string();
    }
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    let proto = h("x-forwarded-proto").unwrap_or("http");
    let host = h("x-forwarded-host").or(h("host")).unwrap_or("localhost");
    format!("{proto}://{host}")
}

#[derive(Deserialize)]
pub struct UpdateQuery {
    /// 插件的 {{bundle_type}}：nsis / deb / appimage
    #[serde(default)]
    bundle: String,
}

/// 按平台和安装方式挑安装包的文件名后缀；不支持的组合 → None
fn package_suffix(target: &str, arch: &str, bundle: &str) -> Option<&'static str> {
    match (target, arch, bundle) {
        ("windows", "x86_64", _) => Some("-setup.exe"),
        ("linux", "x86_64", "deb") => Some("_amd64.deb"),
        ("linux", "x86_64", _) => Some("_amd64.AppImage"),
        _ => None,
    }
}

/// GET /client/update/{target}/{arch}/{current}?bundle=：tauri-plugin-updater 的动态更新格式。
/// 安装包要带同名 .sig；没有对应安装包 → 204（无更新）。
/// 版本比较交给客户端插件（不比当前新就不装）。
pub async fn tauri_update(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Path((target, arch, _current)): Path<(String, String, String)>,
    Query(q): Query<UpdateQuery>,
) -> AppResult<Response> {
    let Some(suffix) = package_suffix(&target, &arch, &q.bundle) else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let r = latest_release(&st).await?;
    let Some(exe) = r.assets.iter().find(|a| a.name.ends_with(suffix)) else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let sig_name = format!("{}.sig", exe.name);
    let Some(sig) = r.assets.iter().find(|a| a.name == sig_name) else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let signature = client()
        .get(&sig.browser_download_url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| AppError::bad_gateway(format!("取签名失败：{e}")))?
        .text()
        .await
        .map_err(|e| AppError::bad_gateway(format!("取签名失败：{e}")))?;
    let s = settings::load(&st.pool).await?;
    let base = origin(&s.public_url, &headers);
    Ok(Json(serde_json::json!({
        "version": version_of(&r.tag_name),
        "notes": r.body,
        "pub_date": r.published_at,
        "url": format!("{base}/v1/client/download/{}", exe.name),
        "signature": signature.trim(),
    }))
    .into_response())
}

/// GET /client/download/{name}：从最新 Release 流式转发一个安装包
pub async fn download(
    State(st): State<SharedState>,
    Path(name): Path<String>,
) -> AppResult<Response> {
    let r = latest_release(&st).await?;
    let a = r
        .assets
        .into_iter()
        .find(|a| a.name == name)
        .ok_or_else(|| AppError::not_found("no_asset"))?;
    let resp = client()
        .get(&a.browser_download_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| AppError::bad_gateway(format!("下载失败：{e}")))?;
    let mut out = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", a.name.replace('"', "")),
        );
    if let Some(len) = resp.content_length() {
        out = out.header(header::CONTENT_LENGTH, len);
    }
    out.body(Body::from_stream(resp.bytes_stream()))
        .map_err(AppError::internal)
}

#[cfg(test)]
mod tests {
    use super::api_base;

    #[test]
    fn repo_url_to_api() {
        assert_eq!(
            api_base("http://192.168.6.210:3000/xiefan/chat").as_deref(),
            Some("http://192.168.6.210:3000/api/v1/repos/xiefan/chat")
        );
        assert_eq!(
            api_base("https://g.example.com/git/o/r/").as_deref(),
            Some("https://g.example.com/git/api/v1/repos/o/r")
        );
        assert_eq!(api_base("http://g.example.com/onlyowner"), None);
        assert_eq!(api_base(""), None);
    }
}
