# Chat

一个私聊 + 群聊的聊天软件。后端 Rust/axum + PostgreSQL，前端 React 单代码库，浏览器 / Windows(Tauri) / Android(Tauri) 三端复用。

## 功能

- 注册登录（用户名 + 可改昵称），JWT access + refresh token
- 好友制：搜索用户名 → 发请求 → 对方同意后成为好友
- 私聊：加好友自动建 DM；删好友后 DM 变只读
- 群聊：只能拉自己的好友；群主可改名/踢人/转让/解散；群主退群自动转让给最早加入成员
- 消息：持久化历史、每会话递增 seq、离线重连按 seq 补发（`sync`）
- 附件：图片/文件消息（multipart 上传 → WS `attachment_id` 发送），图片气泡内联预览、文件卡片下载
- @提及：群聊 @用户名（输入自动补全），气泡高亮 + 会话列表「有人@我」标记，已读后消除
- 会话内搜索：聊天头 🔍 → `GET /conversations/{id}/messages/search?q=`，点结果跳转定位
- 后台管理：最早注册用户自动成为管理员，客户端「管理」tab 提供用户列表/禁用/启用/重置密码/任命管理员 + 全局统计；禁用即踢下线（`force_logout`）且全线拒绝，数据保留、启用恢复；管理员不可被禁用、末位管理员不可撤销
- 系统通知：窗口不在前台时弹系统通知（Web Notification / Tauri），标题栏显示未读数，桌面端任务栏闪烁；静音会话只放行 @我
- 草稿：每个会话的未发送文字自动保存，切换会话、刷新后仍在，会话列表显示「[草稿]」
- 链接与代码：消息里的链接可点击（桌面端用系统浏览器打开），支持行内代码和带复制按钮的代码块
- 暗色模式：跟随系统 / 浅色 / 深色，在「我」页切换
- 存储配额：管理员可设每人附件总量上限，「我」页显示已用空间
- 注册开关：管理员可切换开放注册 / 邀请码 / 关闭注册，并可签发、撤销邀请码
- 登录设备与改密：可查看本账号的登录设备并移出；自助改密码后其他设备立即下线
- 头像：上传 PNG / JPEG / WebP，按版本号更新
- 图片缩略图：较长边超过 480 像素的图片自动生成 JPEG 缩略图
- 语音消息：带时长的音频以语音气泡发送，占位文字为「语音」
- 文件传输助手：每人一个仅自己的会话，用于在多设备间传文件和备忘
- 转发：把消息转发到其他会话（转发到文件传输助手即收藏）；同一次操作重试不产生重复
- 全局搜索：跨会话搜索消息
- 群公告与置顶：群主可设公告；任何成员可置顶消息（每会话最多 20 条）
- 视频会议：会话内打开会议链接（默认 Jitsi，管理员可改地址或关闭）
- 手机推送：管理员配置 ntfy 后，用户在「我」页开启
- 备份与恢复：管理后台一键生成备份（数据库全部表 + 附件 / 头像文件，tar.gz）、下载到本地；上传备份文件恢复（需键入 RESTORE 确认，恢复后所有用户需重新登录）
- Agent：本地 agent（ACP，如 Claude Code、Codex，登记表里可以一键安装）。在专属会话里直接发消息，或在任意会话里 `@名字` 调用。输出可以是实时流式、只显示状态和最终结果，或每步一条消息。权限分只读、可改工作目录、完全放开，超出档位要主人审批。宿主离线时调用会排队。Agent 之间可以互调，也可以定时调用。管理员可以关掉整个功能，或按用户允许、禁止
- 实时：WebSocket（一次性 ticket 鉴权）、多设备同时在线、在线状态、正在输入、未读数、已读同步
- 部署：`docker compose up -d` 一条命令（Postgres + 服务端托管前端，主机端口 28080）

## 快速开始（服务端）

```bash
docker compose up -d          # WSL/任意有 Docker 的环境
# 浏览器打开 http://localhost:28080
```

环境变量：`DATABASE_URL`、`JWT_SECRET`（生产必须改）、`PORT`、`BIND`、`APP_DIST`、`FILES_DIR`（附件存储目录，compose 挂 `chat-files` 卷）、`UPLOAD_MAX_MB`（单文件上限，默认 25）。

## 开发

```bash
# 数据库（主机端口 15432）
docker compose up db -d
# 后端（:28080）
cd server && cargo run
# 前端（:5173，已配置 /v1 代理到 :28080）
pnpm install && pnpm dev
```

## 测试

```bash
pnpm test    # cargo test：auth/好友流/会话权限/seq/WS/群规则 集成测试（需 db 容器）

# E2E 冒烟：直接打运行中的部署（默认 http://localhost:28080）
# 前提：库里已经有管理员。全新部署里第一个注册用户会自动成为管理员；
# 若未设置 E2E_ADMIN_USER，且脚本注册的第一个用户成了管理员，脚本提示后退出 2，
# 避免「非管理员 403」整批误报。管理员流程和注册开关需要 E2E_ADMIN_USER / E2E_ADMIN_PASS。
E2E_ADMIN_USER=<管理员用户名> E2E_ADMIN_PASS=<密码> node scripts/e2e.mjs
docker compose down && docker compose up -d --wait
node scripts/e2e.mjs phase2   # phase2：验证数据持久化（含头像、文件传输助手、公告、置顶、Agent）
```

## 推送（ntfy）

管理员在后台填 ntfy 服务器地址，用户在「我」页开启，并在手机 ntfy App 里订阅页面显示的地址。

## 客户端打包

```bash
cd app
node gen-icon.cjs                              # 已生成，可跳过
./node_modules/.bin/tauri build                # Windows → src-tauri/target/release/bundle/nsis/Chat_0.1.0_x64-setup.exe
./node_modules/.bin/tauri android init         # 首次生成 Android 工程
./node_modules/.bin/tauri android build --apk  # 需 Android SDK + NDK + rust android targets
```

macOS 要在 Mac 上编（仓库根目录先 `pnpm install`，另需 Rust 和 Xcode 命令行工具）：

```bash
cd app
pnpm tauri build --bundles app   # → src-tauri/target/release/bundle/macos/Chat.app
```

把 `Chat.app` 拖进「应用程序」即可。加上 `dmg`（`--bundles app,dmg`）会多打一个 DMG，但这一步要用 AppleScript 控制 Finder，失败时 `Chat.app` 已经生成，直接拖即可。

Tauri 端登录页填写服务器地址（如 `http://192.168.1.5:28080`）。

### Android 在 Windows 上的已知坑

`tauri android build` 里有一步给 `.so` 建符号链接，需要开启 Windows 开发者模式（设置 → 系统 → 开发者选项 → 开发人员模式）。若没开：

```bash
cd app/src-tauri
# 1. 交叉编译 rust lib（以 arm64 为例）
cargo build --target aarch64-linux-android --release
# 2. 手动复制到 jniLibs
mkdir -p gen/android/app/src/main/jniLibs/arm64-v8a
cp target/aarch64-linux-android/release/libchat_app_lib.so gen/android/app/src/main/jniLibs/arm64-v8a/
# 3. 跳过 rustBuild* 直接 gradle 打包
cd gen/android && ./gradlew assembleDebug \
  -x rustBuildArm64Debug -x rustBuildArmDebug -x rustBuildX86Debug -x rustBuildX86_64Debug
# 产物：app/build/outputs/apk/arm64/debug/app-arm64-debug.apk
```

需要 NDK（`sdkmanager "ndk;<ver>"`）+ `rustup target add aarch64-linux-android` 等。

## Agent

桌面端登录后就是这台电脑的 Agent 宿主：自动上线。关掉窗口后留在系统托盘里继续跑。开机自启在「我」页打开，默认关。Web 和 Android 只能管理和调用，不能当宿主。

添加 Agent 有两种方式，都在任意已登录的客户端上完成，不用坐在那台电脑前：

- 从登记表选择（Claude Code、Codex 等）。这台宿主上还没装的，确认后由宿主安装。
- 自定义命令：登记表里没有的程序，手填启动命令和参数。

每个 Agent 固定在添加时选的那台宿主上。权限档位决定什么可以自动做：

- 只读：读取、搜索一类操作自动放行，其余要审批。往其他会话发消息也要审批。
- 可改工作目录：在只读之上，工作目录和额外目录里的修改自动放行。执行命令，以及目录外的写入，要审批。往其他会话发消息自动放行。
- 完全放开：全部自动放行。

超出档位时，审批卡片发到该 Agent 的专属会话，原会话只显示「等待主人批准」。

真 agent 冒烟默认不跑。需要本机已登录 Claude Code 和 Codex，并且 PATH 上有 Node.js（登记表条目若走 npx，会装到临时目录），会消耗 token：

```bash
cd agent-host
CHAT_REAL_AGENT_SMOKE=1 cargo test -p agent-host --test smoke -- --nocapture --test-threads=1
```

每个适配器发一次「只回复 pong」，再发一次往工作目录外写文件的指令。冒烟里权限请求一律拒绝，断言至少拒绝过一次且文件没有写出来。只跑其中一个：`CHAT_REAL_AGENT_SMOKE=codex`（或 `claude`）。

已知限制：Agent 的环境变量和密钥以明文存在服务器上，管理员和备份都能看到。档位只能管到 agent 来请求批准的操作：Codex 的沙箱总是允许写它自己的工作目录，所以 Codex 在「只读」档位下仍能改工作目录里的文件（目录外的写入和联网照样要审批）。macOS / Linux 的桌面安装包要在对应系统上编译，仓库里不附带现成的包。

## 局域网访问

- **同一台机器只跑一个服务端实例**：多实例共享数据库但 WS 连接表各自独立，跨实例的消息只能落库、推不到对端（表现为"必须刷新才看到新消息"）。开发调试也统一用 docker。
- 原生跑 `cargo run`（仅临时调试用）：直接监听 `0.0.0.0:28080`，同网段设备访问 `http://<本机IP>:28080`。用时先停掉 docker server。
- WSL2 里的 `docker compose up`：发布端口只到 Windows localhost。要暴露到局域网需管理员跑一次：
  `netsh interface portproxy add v4tov4 listenport=28080 listenaddress=0.0.0.0 connectport=28080 connectaddress=127.0.0.1`
  （真实部署到 Linux 服务器无此问题，`0.0.0.0` 直接生效）

## 架构

```
app/            React + TS + Vite（Web/Tauri 共用 UI）
  src-tauri/    Tauri v2 壳（Windows/Android）
agent-host/     宿主运行时库（ACP 客户端；桌面端拉起，测试也可直接驱动）
server/         axum + sqlx + PostgreSQL
  src/api.rs    REST：auth/users/friends/conversations/messages/群管理/ws ticket
  src/ws.rs     WS：连接注册、消息广播、presence、typing、read、sync
  src/domain.rs membership / seq / 消息行转换
  migrations/   SQL 迁移（sqlx::migrate! 编译期嵌入）
docker-compose.yml  db + server（托管 app/dist）
```

## 目录约定

- 领域词汇与边界规则：`CONTEXT.md`
- 架构决策记录：`docs/adr/`
