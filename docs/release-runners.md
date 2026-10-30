# GitHub 发版与在线更新全流程

[release.yml](../.github/workflows/release.yml) 有两条入口，跑的是同一条链：

- **合入 main（自动）**：push 动了 `app/src-tauri/tauri.conf.json` 就触发。`gate` 读出
  version，若 `v<version>` 还没发过版，`tag` job 自动建注解标签（说明取合并 PR 的
  标题+正文，找不到就用 commit message），同一 run 内继续发版——GITHUB_TOKEN 推的
  tag 不会再触发工作流，所以必须在同一个 run 完成。version 没变（tag/Release 已存在）
  就干净跳过，不会重复发版。
- **手动推 `v*` 标签（特殊场合）**：照旧发版，标签必须等于 `v`+version，标签注解
  就是客户端看到的更新说明。

链路：gate 查重 → 复用 [ci.yml](../.github/workflows/ci.yml) 的测试 → 需要时建 tag →
建草稿 Release → 三台机器并行打包、上传到草稿 → 验齐 7 个产物后自动转公开。

| job | runner | 产物 |
|---|---|---|
| gate | ubuntu-latest | 读 version、查重（tag 是否已存在、Release 是否已发布），输出 tag / sha / 是否要建 tag |
| test | ubuntu-latest | tsc + vitest + cargo test（postgres:17 服务容器，复用 ci.yml） |
| tag | ubuntu-latest | 仅自动路径：建注解 tag `v<version>` 并推 origin（说明 = 合并 PR 标题+正文） |
| draft | ubuntu-latest | 校验标签 == `v`+version，按标签注解建草稿 Release（已存在则沿用；已发布则干净收队） |
| windows | windows-latest | `Chat_X_x64-setup.exe` + `.sig`（NSIS，Tauri 签名） |
| android | ubuntu-latest | `Chat_X_arm64.apk`（debug 签名，覆盖安装要求同一个 keystore） |
| linux | ubuntu-22.04 | `Chat_X_amd64.deb` + `.sig`、`Chat_X_amd64.AppImage` + `.sig` |
| publish | ubuntu-latest | 7 个产物齐了才 `gh release edit --draft=false`；仅最高版本带 `--latest` |

全部用免费托管 runner，不花钱、不需要自备机器。草稿对外不可见（`/releases/latest` 忽略草稿），
所以打坏一半也不会被客户端看到；修好到 Actions 页 **Re-run failed jobs** 即可，tag 和草稿
都会被沿用，已发布的 Release 绝不会再被覆盖上传。

发版 run 全程串行（concurrency 组 `release-<repo>`，`cancel-in-progress: false`）。注意
GitHub 每个组只保留**一个**排队中的 run：一个 release run 正在跑时又连续触发两次，先排队的
那个会被后到的顶掉（显示为 cancelled）。碰到这种情况重推或重跑即可，正在跑的 run 不受影响。

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
# 2. 合入即触发 release：gate 看到 v0.5.1 没发过 → test → 自动建注解 tag v0.5.1
#    （说明 = 合并 PR 的标题+正文）→ draft → 打包 → publish。全程不用手动打 tag
```

到仓库 Actions 页看 `release` 工作流全绿后，`v0.5.1` 出现在 Releases（标 Latest），
「我」页的「应用更新」随即能查到。

重跑：某个 job 挂了，修好直接 Re-run failed jobs——已建的 tag、草稿都沿用，`--clobber`
覆盖同名资产；标签或版本不一致时 gate/draft 会拒绝。

手动 tag（特殊场合，如自定义更新说明、给未合入的 commit 单独发版）：

```bash
git tag -a v0.5.1 -m "更新说明"
git push origin v0.5.1
```

标签必须等于 `v`+version（否则 gate 拒绝），注解就是更新说明。注意两点：

- 给「未合入」的 commit 打 tag，发布的就是那个 commit。
- 别给一个「即将合入」的版本抢先手动打 tag：合入后那次 release run 会发现 tag 指向
  别的 commit 而告警退出（若手动那次已发完，则是「已发布」直接跳过）。想要自定义
  更新说明才值得这么做。

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
| 后台 → 服务器对外地址 | `public_url`：更新响应里下载地址的前缀（留空按请求 Host 拼），也是推送通知的 Click 跳转地址。代码默认 `https://chat.287152.xyz`；存量库里的旧值不会自动变，需管理员改一次 |

docker-compose 部署的话这几个变量写在 `docker-compose.yml` 的 environment 里（已留注释样例）。

## 出问题时先看这里

| 现象 | 原因 / 处理 |
|---|---|
| 合入 main 但没发版 | 看 run 顶部 gate 的 Summary：version 没变（`v<ver>` 的 tag/Release 已存在）属正常；没动 `tauri.conf.json` 的 push 不触发也属正常；fork 没开 `AUTO_RELEASE` 见下 |
| gate 警告「tag 指向另一个 commit」 | 有人手动打了同名 tag 指向别的 commit；要发完那次请到原 run Re-run failed jobs，或删掉该 tag 重推 |
| gate 红：`标签与 version 不一致` | 手动 tag 必须是 `v` + `app/src-tauri/tauri.conf.json` 的 version，如 `v0.5.1` ↔ `0.5.1` |
| tag job 红：推 tag 失败 | 并发/手动抢建了同名 tag 且指向别的 commit（日志里会给出两边 sha），或仓库 tag ruleset 没放行 github-actions |
| 排队中的 release run 变 cancelled | concurrency 组只留一个排队 run，被后到的触发顶掉了；重推或 Re-run，正在跑的 run 不受影响 |
| windows/linux job 红：密钥未配置 | 没加 `TAURI_SIGNING_PRIVATE_KEY`，或加错名字；空值也会失败（不允许静默跳过 `.sig`） |
| APK 有但手机装不上 / 签名冲突 | `ANDROID_DEBUG_KEYSTORE` 不是以前打包那台电脑的 keystore |
| publish 红：缺资产 | 某个上传没成功；看对应 job 日志，修好 Re-run failed jobs |
| 客户端查不到新版本 | Release 还是草稿（publish 没跑完），或服务端 `update_repo_url` 指错仓库；服务端日志看「更新源」报错 |
| `/v1/client/latest` 返回 Cloudflare 纯文本 `error code: 502`、无 CORS | 源站对更新源失败曾回 HTTP 502，被 CF Error Pages 换成纯文本（丢 JSON/CORS）；已改为回 503。若仍失败看服务端日志「更新源不可达」，并配置 `CLIENT_UPDATE_PROXY`（国内机访问 GitHub 常需要） |
| 装的是 deb，更新时要输入密码 | 正常现象：deb 更新要用 root 权限执行 `dpkg -i`。用 AppImage 装的不需要密码 |
| fork 上想试跑 | 手动推 `v*` tag 即可走全流程（签名校验会明确报错，把 `TAURI_SIGNING_PRIVATE_KEY` 填任意值可看流程走通，产物不能用于真实更新）。想试**自动路径**：fork 仓库 Settings → Variables → Actions 加 `AUTO_RELEASE=true`，再在某个测试分支的临时 commit 里把该分支名加进 `release.yml` 的 `on.push.branches`（用完丢弃、别合回 main），推这个分支就会触发 gate 的自动路径 |
