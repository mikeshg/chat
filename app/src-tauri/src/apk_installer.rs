//! AS2：Android 应用内 APK 安装器（In-app APK Installer）——
//! 下载进应用私有缓存、FileProvider content URI 交系统安装器。
//!
//! 与 MM4 附件下载（MediaStore 公共 Downloads）是完全独立的路径：
//! URL 白名单与文件名净化仍在 Rust 侧完成（复用附件同款策略），
//! Kotlin ApkInstallerPlugin 只做缓存写盘、进度快照和起安装 Intent。
//!
//! 错误约定：Kotlin reject 文案带稳定前缀，前端据此区分——
//!   PERMISSION_DENIED:…  未授予「安装未知应用」权限（AS3 弹引导 + 浏览器兜底）
//!   INSTALL_FAILED:…     系统安装器拉起失败（无安装器、FileProvider 不覆盖）
//!   其余                 下载 / 安装流程失败（网络、IO、参数、包不存在）

#[cfg(target_os = "android")]
use tauri::Manager; // manage/try_state

use crate::download::sanitize_file_name;

/// download_update_apk 的结果：path = 缓存内绝对路径，name = 缓存文件名（安装时回传它）
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ApkDownloaded {
    pub path: String,
    pub name: String,
}

/// apk_download_progress 的轮询快照（state: idle/downloading/done/error；total -1 = 未知）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApkDownloadProgress {
    pub state: String,
    pub received_bytes: u64,
    pub total_bytes: i64,
    pub name: String,
    pub error: String,
}

#[derive(Debug, serde::Deserialize)]
struct PermStatus {
    granted: bool,
}

/// Android 插件句柄的受管状态（newtype，避免与别的插件同型 state 互撞，同 download.rs）
#[cfg(target_os = "android")]
struct ApkInstallerHandle<R: tauri::Runtime>(tauri::plugin::PluginHandle<R>);

/// Tauri 插件壳：仅在 Android 上注册 Kotlin ApkInstallerPlugin（com.chat.app.ApkInstallerPlugin）
pub fn init<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("apk_installer")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            {
                let handle = api.register_android_plugin("com.chat.app", "ApkInstallerPlugin")?;
                app.manage(ApkInstallerHandle(handle));
            }
            #[cfg(not(target_os = "android"))]
            let _ = (app, api);
            Ok(())
        })
        .build()
}

#[tauri::command]
pub async fn download_update_apk(
    app: tauri::AppHandle,
    url: String,
    allowed_origins: Vec<String>,
) -> Result<ApkDownloaded, String> {
    let (url, name) = validate_apk_url(&url, &allowed_origins)?;
    platform_call(
        &app,
        "download",
        serde_json::json!({ "url": url.as_str(), "fileName": name }),
    )
    .await
}

#[tauri::command]
pub async fn apk_download_progress(app: tauri::AppHandle) -> Result<ApkDownloadProgress, String> {
    platform_call(&app, "progress", serde_json::json!({})).await
}

#[tauri::command]
pub async fn check_install_permission(app: tauri::AppHandle) -> Result<bool, String> {
    let s: PermStatus = platform_call(&app, "checkInstallPermission", serde_json::json!({})).await?;
    Ok(s.granted)
}

#[tauri::command]
pub async fn open_install_permission_settings(app: tauri::AppHandle) -> Result<(), String> {
    platform_call::<serde_json::Value>(&app, "openInstallPermissionSettings", serde_json::json!({}))
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn install_update_apk(app: tauri::AppHandle, name: String) -> Result<(), String> {
    platform_call::<serde_json::Value>(
        &app,
        "install",
        serde_json::json!({ "fileName": name }),
    )
    .await
    .map(|_| ())
}

// ---------- 平台分发 ----------

#[cfg(target_os = "android")]
async fn platform_call<T: serde::de::DeserializeOwned>(
    app: &tauri::AppHandle,
    command: &str,
    payload: serde_json::Value,
) -> Result<T, String> {
    use tauri::plugin::mobile::PluginInvokeError;
    let handle = app
        .try_state::<ApkInstallerHandle<tauri::Wry>>()
        .ok_or_else(|| "更新组件未就绪".to_string())?;
    handle
        .0
        .run_mobile_plugin_async::<T>(command, payload)
        .await
        .map_err(|e| match e {
            PluginInvokeError::InvokeRejected(r) => {
                r.message.unwrap_or_else(|| "更新失败".to_string())
            }
            other => format!("更新失败：{other}"),
        })
}

#[cfg(not(target_os = "android"))]
async fn platform_call<T: serde::de::DeserializeOwned>(
    _app: &tauri::AppHandle,
    _command: &str,
    _payload: serde_json::Value,
) -> Result<T, String> {
    Err("当前平台不支持应用内安装".into())
}

// ---------- 安装包地址校验 ----------

const BAD_APK_URL: &str = "安装包地址无效";

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// path 段的百分号解码：只认 %XX；'+' 在 path 里就是字面加号（不是空格）
fn percent_decode(seg: &str) -> Result<String, String> {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(BAD_APK_URL.into());
            }
            let (h, l) = match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => (h, l),
                _ => return Err(BAD_APK_URL.into()),
            };
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| BAD_APK_URL.to_string())
}

/// 仅放行服务器安装包地址：http/https、路径恰为 [<服务器前缀>/]v1/client/download/<名>、
/// 名单段解码后是不含分隔符、非 . / .. 的 .apk 文件名；无 userinfo/fragment/query。
/// 返回（校验过的 URL, 净化后的缓存文件名）。
fn validate_apk_url(raw: &str, allowed_origins: &[String]) -> Result<(url::Url, String), String> {
    let mut u = url::Url::parse(raw.trim()).map_err(|_| BAD_APK_URL.to_string())?;
    if !matches!(u.scheme(), "http" | "https")
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
    {
        return Err(BAD_APK_URL.into());
    }
    u.set_fragment(None);
    for allowed in allowed_origins {
        let Ok(a) = url::Url::parse(allowed) else {
            continue;
        };
        if a.origin() != u.origin() {
            continue;
        }
        let base = a.path().trim_end_matches('/');
        let Some(seg) = u
            .path()
            .strip_prefix(&format!("{base}/v1/client/download/"))
        else {
            continue;
        };
        // 单段：余下部分不能再含 '/'
        if seg.is_empty() || seg.contains('/') {
            return Err(BAD_APK_URL.into());
        }
        let decoded = percent_decode(seg)?;
        // 拒绝目录穿越意图；"."、".." 交给净化器也会变 fallback，这里直接挡
        if decoded.contains('/') || decoded.contains('\\') || decoded == ".." {
            return Err(BAD_APK_URL.into());
        }
        let name = sanitize_file_name(&decoded);
        if !name.to_ascii_lowercase().ends_with(".apk") {
            return Err("不是安装包文件".into());
        }
        return Ok((u, name));
    }
    Err("安装包地址不在允许的服务器列表".into())
}

#[cfg(test)]
mod tests {
    use super::validate_apk_url;

    const ORIGIN: &str = "https://chat.example.com";

    fn ok_url(path: &str) -> String {
        format!("{ORIGIN}{path}")
    }

    fn allowed() -> Vec<String> {
        vec![ORIGIN.to_string()]
    }

    #[test]
    fn accepts_release_apk_url() {
        let (u, name) = validate_apk_url(
            &ok_url("/v1/client/download/Chat_0.7.1_arm64.apk"),
            &allowed(),
        )
        .unwrap();
        assert_eq!(name, "Chat_0.7.1_arm64.apk");
        assert!(u.as_str().ends_with("/Chat_0.7.1_arm64.apk"));
    }

    #[test]
    fn accepts_prefixed_server_and_encoded_name() {
        let (_u, name) = validate_apk_url(
            "http://h:9/sub/v1/client/download/Chat%200.7.1.apk",
            &["http://h:9/sub".to_string()],
        )
        .unwrap();
        assert_eq!(name, "Chat 0.7.1.apk");
    }

    #[test]
    fn fragment_is_stripped_not_rejected() {
        // fragment 不进 HTTP 请求，剥掉即可（与附件校验同款）
        validate_apk_url(&ok_url("/v1/client/download/Chat.apk#x"), &allowed()).unwrap();
    }

    #[test]
    fn rejects_non_apk_and_traversal() {
        let cases = [
            ok_url("/v1/client/download/Chat_0.7.1_x64-setup.exe"),
            ok_url("/v1/client/download/Chat_0.7.1_arm64.apk.sig"),
            ok_url("/v1/client/download/sub/Chat.apk"),            // 多一段
            ok_url("/v1/client/download/"),                        // 空名
            ok_url("/v1/client/download/..%2f..%2fetc.apk"),       // 解码后含 /
            ok_url("/v1/client/download/a%5cb.apk"),               // 解码后含 \
            ok_url("/v1/client/download/%zz.apk"),                 // 坏百分号
            ok_url("/v1/client/download/Chat.apk?x=1"),            // 带 query
            "https://evil.example/v1/client/download/Chat.apk".into(),
            "http://u:p@chat.example.com/v1/client/download/a.apk".into(),
            "ftp://chat.example.com/v1/client/download/a.apk".into(),
            "not a url".to_string(),
        ];
        for u in cases {
            assert!(validate_apk_url(&u, &allowed()).is_err(), "{u}");
        }
    }
}
