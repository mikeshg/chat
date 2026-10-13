# GitHub 发版与在线更新全流程

推 `v*` 标签后，[release.yml](../.github/workflows/release.yml) 在 GitHub 免费托管 runner 上跑完
整条链：复用 [ci.yml](../.github/workflows/ci.yml) 的测试 → 建草稿 Release → 三台机器并行打包、
上传到草稿 → 验齐 7 个产物后自动转公开。

| job | runner | 产物 |
|---|---|---|
| test | ubuntu-latest | tsc + vitest + cargo test（postgres:17 服务容器） |
| draft | ubuntu-latest | 校验标签 == `v`+version，按标签注解建草稿 Release |
| windows | windows-latest | `Chat_X_x64-setup.exe` + `.sig`（NSIS，Tauri 签名） |
| android | ubuntu-latest | `Chat_X_arm64.apk`（debug 签名，覆盖安装要求同一个 keystore） |
| linux | ubuntu-22.04 | `Chat_X_amd64.deb` + `.sig`、`Chat_X_amd64.AppImage` + `.sig` |
| publish | ubuntu-latest | 7 个产物齐了才 `gh release edit --draft=false --latest` |

全部用免费托管 runner，不花钱、不需要自备机器。草稿对外不可见（`/releases/latest` 忽略草稿），
所以打坏一半也不会被客户端看到；修好重跑失败的 job 即可。

## 0. 一次性准备：Secrets 与钥匙备份

在仓库 `xie-fan/chat` → Settings → Secrets and variables → Actions → New repository secret 加：

| Secret | 必填 | 内容 |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | 是 | 开发机上 `~/.tauri/chat-updater.key` 的全部文本。为空时 windows/linux job 直接报错：没有它产不出 `.sig`，在线更新装不上 |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 否 | 私钥口令；没设过就留空 |
| `ANDROID_DEBUG_KEYSTORE` | 否（正式发版要） | 以前打 APK 那台电脑上 debug keystore 的 base64：<br>`base64 -w0 ~/.android/debug.keystore`（Linux）<br>`[Convert]::ToBase64String([IO.File]::ReadAllBytes("$HOME\.android\debug.keystore"))`（PowerShell）<br>不设会改用自动生成的 debug key，仅供 fork 测试，**覆盖不了已装的正式包**（Android 会报签名冲突） |

把 `chat-updater.key`（与 `app/src-tauri/tauri.conf.json` 里的 updater pubkey 配对）和
`debug.keystore` 备份到安全的地方。丢了私钥就再也推不了在线更新，只能让大家手动重装；
丢了 keystore，Android 用户必须卸载重装。

## 1. 发一个版本

```bash
# 1. 改 app/src-tauri/tauri.conf.json 的 version（如 0.5.1），提交合入 main
#    bump 规则：修 bug/小改 bump patch，新功能 bump minor
# 2. 打标签并推送；标签必须等于 v+version，标签说明就是客户端看到的更新说明
git tag -a v0.5.1 -m "更新说明"
git push origin v0.5.1
```

推完到仓库 Actions 页看 `release` 工作流：test → draft → 三个打包 job → publish，
全绿后 `v0.5.1` 出现在 Releases（标 Latest），「我」页的「应用更新」随即能查到。

重跑：某个打包 job 挂了，修好直接 Re-run failed jobs——draft 已存在会沿用，`--clobber`
覆盖同名资产；标签或版本不一致时 draft job 会拒绝。

## 2. 服务端设置（在线更新那一半）

服务端读 GitHub 最新 Release 转给客户端（`server/src/client_update.rs`）。默认配置
`xie-fan/chat` 开箱即用，不用进后台设置；要改才动这些：

| 配置 | 说明 |
|---|---|
| 后台 → 客户端在线更新 | `update_repo_url`：`owner/repo` 或 `https://github.com/owner/repo` → GitHub；其他 http(s) 地址 → Gitea（兼容旧自建）；`off` 关闭；留空用 `CLIENT_UPDATE_REPO` |
| `CLIENT_UPDATE_REPO` | DB 设置为空时的兜底，默认 `xie-fan/chat`；`off` = 彻底关闭 |
| `GITHUB_TOKEN` | 可选。读 release 时带 `Authorization: Bearer`，只吃 API 配额用；只发给 API host，资产走 `browser_download_url` 302 到 CDN 时不带凭据 |
| `CLIENT_UPDATE_PROXY` | 可选。更新请求走 HTTP 代理，如 `http://127.0.0.1:7890`；不设则不走系统代理 |
| `GITHUB_API_BASE` | 可选。默认 `https://api.github.com`，测试 mock 用 |

docker-compose 部署的话这几个变量写在 `docker-compose.yml` 的 environment 里（已留注释样例）。

## 出问题时先看这里

| 现象 | 原因 / 处理 |
|---|---|
| draft job 红：`标签与 version 不一致` | 标签名必须是 `v` + `app/src-tauri/tauri.conf.json` 的 version，如 `v0.5.1` ↔ `0.5.1` |
| windows/linux job 红：密钥未配置 | 没加 `TAURI_SIGNING_PRIVATE_KEY`，或加错名字；空值也会失败（不允许静默跳过 `.sig`） |
| APK 有但手机装不上 / 签名冲突 | `ANDROID_DEBUG_KEYSTORE` 不是以前打包那台电脑的 keystore |
| publish 红：缺资产 | 某个上传没成功；看对应 job 日志，修好 Re-run failed jobs |
| 客户端查不到新版本 | Release 还是草稿（publish 没跑完），或服务端 `update_repo_url` 指错仓库；服务端日志看「更新源」报错 |
| 装的是 deb，更新时要输入密码 | 正常现象：deb 更新要用 root 权限执行 `dpkg -i`。用 AppImage 装的不需要密码 |
| fork 上想试跑 | 加不了真 secrets 也能跑：签名校验会明确报错；把 `TAURI_SIGNING_PRIVATE_KEY` 填任意值可看流程走通（产物不能用于真实更新） |
