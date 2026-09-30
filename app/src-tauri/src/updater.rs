//! 在线更新（仅桌面）：更新地址跟着用户填的服务器走，只能运行时设置。
//! 服务器可能是明文 HTTP（局域网），安全性靠安装包签名（tauri.conf.json 里的 pubkey）。
use tauri_plugin_updater::UpdaterExt;

/// 检查并安装；没有更新返回 false。Windows 下安装器启动后本进程会退出。
#[tauri::command]
pub async fn install_update(app: tauri::AppHandle, server: String) -> Result<bool, String> {
    let url = format!(
        "{}/v1/client/update/{{{{target}}}}/{{{{arch}}}}/{{{{current_version}}}}?bundle={{{{bundle_type}}}}",
        server.trim_end_matches('/')
    );
    let endpoint = url.parse().map_err(|e| format!("服务器地址无效：{e}"))?;
    let update = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    let Some(update) = update else {
        return Ok(false);
    };
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}
