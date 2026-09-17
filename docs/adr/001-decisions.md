# ADR-001：关键技术决策

## 1. 单 React 代码库 + Tauri 壳

**决策**：Web/Android/Windows 三端共用一套 React SPA；Tauri v2 打包桌面与安卓。

**理由**：避免三套 UI；Tauri 比 Electron 体积小一个数量级；登录页暴露「服务器地址」字段解决客户端→LAN 服务器寻址。

**代价**：Tauri Android 需要本机 SDK+NDK；iOS 不在范围。

## 2. REST + WebSocket 混合

**决策**：读写状态走 REST（好友、群管理、历史分页）；实时通道走 WS（消息收发、presence、typing、read、sync）。WS 用一次性 ticket（30s 有效、用后即焚）鉴权，不把长期 token 放进 query 之外的持久位置。

**理由**：消息高频双向 → WS 天然合适；好友/群操作低频且幂等语义强 → REST。ticket 隔离 WS 鉴权与 HTTP 鉴权，token 不出现在 URL 里被代理记录的风险最小化（ticket 是一次性的）。

## 3. 每会话单调 seq 做同步锚点

**决策**：`messages.seq` 由 Postgres `conversations.last_seq` 在事务内 `UPDATE ... RETURNING` 分配；断线重连用 `sync(last_seq)` 拉增量。

**理由**：比时间戳可靠（无并发/时钟问题）；客户端本地只需记住 last_seq；已读位置也用 seq（`memberships.last_read_seq`），未读数 = `last_seq - last_read_seq` 一条 SQL 算出。

## 4. 好友制而非开放 DM

**决策**：DM 只能发生在互为好友的用户间；删好友 → DM 会话保留但只读（WS 层在 `send_message` 时校验好友关系仍然成立）。

**理由**：符合「真实产品」的骚扰模型；只读而非删除保留了历史证据链。

## 5. 群组规则

- 只能拉自己的好友入群（同理反垃圾）。
- 群主退出 → 自动转让给最早加入的成员；最后一人退出 → 群解散（删会话）。
- 解散保留消息但全员移出（membership 删除，历史不可再写）。
- 踢人/改名/转让/解散仅群主（v1 无 admin 角色）。

## 6. 传输加密边界

**决策**：仅 TLS/WSS，不做 E2E 加密。本机/局域网部署时为 ws:// 明文——接受此风险，公网部署须挂 TLS（Caddy/nginx 反代或 axum rustls）。

## 7. postgresql + sqlx 编译期嵌入迁移

`sqlx::migrate!("./migrations")` 在编译期把 SQL 打进二进制，容器镜像无需拷贝 SQL 目录。开发库与测试库分离（`chat` / `chat_test`，测试自动建库）。

## 8. 多设备在线模型

`AppState.conns: user_id → {conn_id → sender}`。同一账号多连接并存；presence 只在「首连/末断」时翻转广播；read_sync 把已读状态推给同账号其他设备（清角标）。
