# A25 多标签页刷新令牌不互相清空

来源：2026-09-28 第二轮代码审查（基于 `5317e8d`）第 4 条，主会话已核对属实。先读 [../devin-rules.md](../devin-rules.md)。本票**只改 `app/`**，没有迁移，不改服务端。

## 问题

`app/src/api.ts` 的 `tryRefresh` 用模块内的 `refreshing` 合并同一页面里的并发刷新，但访问令牌和刷新令牌存在所有标签页共享的 `localStorage` 里。

两个标签页的访问令牌同时过期（例如电脑睡眠醒来后）：

1. 两边都读到同一个旧刷新令牌，各自 `POST /v1/auth/refresh`；
2. 服务端轮换令牌：A 成功，把新令牌写进 `localStorage`；B 用的旧令牌已被消费，收到 401（服务端只回 401，不会撤销整个会话，A 的新令牌仍然有效）；
3. B 的 `call()` 看到刷新失败，无条件 `clearSession()`，把 A 刚写入的有效凭据删掉 → 两个标签页都被登出。

## 要求

1. 跨标签页串行刷新：浏览器支持时用 Web Locks（`navigator.locks.request`）包住刷新。拿到锁后先重读 `localStorage` 里的刷新令牌：已经不是这次请求开始时用的那个（别的标签页刚刷新过）→ 不再请求服务端，直接当作刷新成功，用新的访问令牌重试原请求。
2. 不支持 Web Locks 时的退路：刷新失败后，如果 `localStorage` 里的刷新令牌已经被换成别的值，同样当作刷新成功，不清空会话。
3. 只有确认凭据确实失效（刷新令牌没被别人换掉、服务端也拒绝了）才 `clearSession()` 并派发 `chat:logout`。
4. 现有的会话代次（`session()` / `requireSession`）语义不变：同一页面里登出、换服务器后，旧请求不能把旧凭据写回。

## 测试（vitest）

参照 `app/src/api.test.ts` 的 `localStorage` 桩：

- 两个独立的 `api.ts` 模块实例（`vi.resetModules()` 后分别导入）共享同一份存储，假后端按顺序轮换令牌：A 刷新成功、B 刷新 401 → 存储里保留 A 的新令牌，B 的原请求用新令牌重试成功，没有派发 `chat:logout`。
- 有 Web Locks 桩时：两个实例的刷新被串行化，服务端只收到一次刷新请求。
- 刷新令牌没被换掉、服务端 401 → 仍然清空会话并派发 `chat:logout`。

## 检查

devin-rules §4：app 两项（`npx vitest run`、`npm run build`）；本票不改 `server/`、`agent-host/`、`src-tauri`，跳过那几项并在交付文档写明。

## 不做

- 跨标签页同步登出 / 登录状态（`storage` 事件监听）。
