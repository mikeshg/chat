# A24 宿主连接复核登录会话

来源：2026-09-28 第二轮代码审查（基于 `5317e8d`）第 3 条，主会话已核对属实。先读 [../devin-rules.md](../devin-rules.md)、`CONTEXT.md`。本票**只改 `server/`**，没有迁移。

## 问题

普通 WS 连接（`server/src/ws.rs`）的做法是：HTTP 升级前复核一次；`register_conn` 之后再调用 `connection_valid` 复核一次，堵住「升级后、登记前」的缝隙；之后每收到一帧都复核（账号未禁用、`session_version` 未变、所属登录会话未撤销）。

宿主连接（`server/src/host_ws.rs` 的 `host_session`）只有升级前那一次：

- 升级后最多等 10 秒 `host_hello`，然后 `register_host`，中间不复核。这段时间里撤销该设备（`kick_session` → `disconnect_hosts`）只会断开已登记的宿主，漏掉这条连接；它随后照常登记。
- 登记后的读循环从不复核。

后果：被撤销的设备（例如被偷的电脑）可以一直作为宿主在线，收到之后的调用 prompt、附件能力 URL 和调用凭据。原版客户端几毫秒内就发 hello，但改过的客户端可以把 hello 拖到接近 10 秒，把窗口放大。

## 要求

1. `register_host` 之后立刻按与 `connection_valid` 相同的条件复核（账号未禁用、`session_version` 一致、`sid` 对应的会话未撤销；`sid` 为空的老连接不查会话），不通过就 `unregister_host`、关闭连接，不发 `host_ready`、不补发排队调用、不广播上线。
2. 读循环里每次 ping 时复核一次（间隔见 `ws_ping_interval_ms`）；不要每一帧都查库，流式事件大约每 100ms 一帧。复核失败按正常断开处理（走现有的离线收尾）。
3. 复核的 SQL 和 `ws.rs` 共用一份，不要复制粘贴。

## 测试（`it.rs`）

- 宿主角色连上后先不发 `host_hello`，撤销该登录会话（`DELETE /v1/users/me/sessions/{id}`，走 `kick_session`），再发 hello → 连接被关闭，没有收到 `host_ready`，宿主不在线。
- 已登记的宿主：直接在库里把会话标成撤销（绕开 `kick_session`，模拟漏网），把 ping 间隔调小 → 在下一次 ping 后连接断开、宿主离线。
- 正常路径不受影响（现有宿主测试全部通过）。

## 检查

devin-rules §4：server 两项（`cargo fmt && cargo test --tests`）、app 两项；本票不改 `agent-host/` 和 `src-tauri`，跳过那两项并在交付文档写明。

## 不做

- 宿主掉线补发、取消竞态（A23）。
