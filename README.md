# Chat

一个私聊 + 群聊的聊天软件。后端 Rust/axum + PostgreSQL，前端 React 单代码库，浏览器 / Windows(Tauri) / Android(Tauri) 三端复用。

## 功能

- 注册登录（用户名 + 可改昵称），JWT access + refresh token
- 好友制：搜索用户名 → 发请求 → 对方同意后成为好友
- 私聊：加好友自动建 DM；删好友后 DM 变只读
- 群聊：只能拉自己的好友；群主可改名/踢人/转让/解散；群主退群自动转让给最早加入成员
- 消息：持久化历史、每会话递增 seq、离线重连按 seq 补发（`sync`）
- 实时：WebSocket（一次性 ticket 鉴权）、多设备同时在线、在线状态、正在输入、未读数、已读同步
- 部署：`docker compose up -d` 一条命令（Postgres + 服务端托管前端，单端口 8080）

## 快速开始（服务端）

```bash
docker compose up -d          # WSL/任意有 Docker 的环境
# 浏览器打开 http://localhost:8080
```

环境变量：`DATABASE_URL`、`JWT_SECRET`（生产必须改）、`PORT`、`BIND`、`APP_DIST`。

## 开发

```bash
# 数据库
docker compose up db -d
# 后端（:8080）
cd server && cargo run
# 前端（:5173，已配置 /v1 代理到 :8080）
pnpm install && pnpm dev
```

## 测试

```bash
pnpm test    # cargo test：auth/好友流/会话权限/seq/WS/群规则 集成测试（需 db 容器）
```

## 客户端打包

```bash
cd app
node gen-icon.cjs                              # 已生成，可跳过
./node_modules/.bin/tauri build                # Windows → src-tauri/target/release/bundle/nsis/Chat_0.1.0_x64-setup.exe
./node_modules/.bin/tauri android init         # 首次生成 Android 工程
./node_modules/.bin/tauri android build --apk  # 需 Android SDK + NDK + rust android targets
```

Tauri 端登录页填写服务器地址（如 `http://192.168.1.5:8080`）。

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

## 局域网访问

- 原生跑 `cargo run`：直接监听 `0.0.0.0:8080`，同网段设备访问 `http://<本机IP>:8080`。
- WSL2 里的 `docker compose up`：发布端口只到 Windows localhost。要暴露到局域网需管理员跑一次：
  `netsh interface portproxy add v4tov4 listenport=8080 listenaddress=0.0.0.0 connectport=8080 connectaddress=127.0.0.1`
  （真实部署到 Linux 服务器无此问题，`0.0.0.0` 直接生效）

## 架构

```
app/            React + TS + Vite（Web/Tauri 共用 UI）
  src-tauri/    Tauri v2 壳（Windows/Android）
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
