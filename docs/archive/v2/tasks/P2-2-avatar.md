# P2-2 头像上传 + 统一 Avatar 组件

先读 [../devin-rules.md](../devin-rules.md) 和 [P2-0-media-schema.md](P2-0-media-schema.md)（`users.avatar_path/avatar_version`、`UserPublic.avatar_version/has_avatar`、`MessageView.sender_avatar_version/sender_has_avatar` 已由主会话建好，**不要再改 `message_views`**）。迁移号：不需要。

## 背景

所有头像都是首字母方块（`ConvList`、`ChatView` 消息行、`MePanel`、`ChatScreen` 侧栏、`ContactsPanel`、`GroupInfo`、`AdminPanel` 各写了一份）。小团队里同名、同首字母的人很难区分。

## 规则

- 用户只能改自己的头像。格式 PNG / JPEG / WebP，≤ 1MB。服务端用文件头魔数校验真实格式（不信任客户端 MIME），不需要解码图片。
- 存储：`FILES_DIR/avatars/<user_id>-<version>.<ext>`（目录不存在就建）。更换成功后 `avatar_version + 1`、写 `avatar_path`，提交后异步删除旧文件。
- 删除头像：`avatar_path = NULL`、`avatar_version + 1`（版本号仍递增，保证客户端缓存失效）。
- `avatar_version = 0` 且 `avatar_path` 为 NULL 表示从未设置；是否有头像以 `avatar_path` 为准，对外只暴露 `avatar_version` 和一个布尔（见下）。

## 接口

- `PUT /v1/users/me/avatar`：multipart，字段 `file`。成功返回更新后的 `UserPublic`。格式不对 400，超过 1MB 400（路由单独设 body 上限，别用附件的 25MB）。
- `DELETE /v1/users/me/avatar` → 更新后的 `UserPublic`。
- `GET /v1/users/{id}/avatar?v=<version>`：**无需鉴权**（`<img>` 带不了 Authorization；用户 id 是 UUID，头像属于低敏感信息，与附件的能力 URL 模型一致）。有头像返回图片，`v` 等于当前版本时 `Cache-Control: public, max-age=31536000, immutable`，否则 `no-cache`；没有头像 404。加 `X-Content-Type-Options: nosniff`。
- `UserPublic.avatar_version / has_avatar` 与 `MessageView.sender_avatar_version / sender_has_avatar` 已由 P2-0 全部接好（所有构造 `UserPublic` 的查询都带上了），你只需要让上传 / 删除正确写入 `users.avatar_path / avatar_version`。
- 头像变更后，给该用户的 watchers（`AppState::watchers`）和本人其他连接发 `invalidate { what: "conversations" }`，对方刷新目录后拿到新版本。

## 客户端

- 新组件 `app/src/ui/Avatar.tsx`：`<Avatar userId name avatarVersion hasAvatar size="sm" | "md" | "lg" />`。
  - 有头像：`<img src=".../v1/users/{id}/avatar?v={version}">`（服务器地址用 `api.ts` 现有的拼接方式，Tauri 端也要能用），`loading="lazy"`，加载失败回落到首字母。
  - 无头像：保持现在的首字母样式（颜色、字号不变）。
- store 维护 `avatars: Record<userId, { version: number; has: boolean }>`：从任何收到的 `UserPublic`（本人、好友、会话 peer、群成员列表）更新，只接受更大的版本；消息行用消息自带的 `sender_avatar_version / sender_has_avatar` 与 store 里的值比较，取版本较大的那组。加载 404 时回落首字母（并把该用户记为无头像，避免反复请求）。
- 用 `Avatar` 替换所有现有首字母头像：`ConvList`（私聊显示对方头像；群聊保持首字母 / 群图标）、`ChatView` 消息行、`MePanel`、`ChatScreen` 侧栏、`ContactsPanel`、`GroupInfo` 成员列表、`AdminPanel` 用户列表。保留原有的在线点等叠加元素。
- 「我」页头像可点击 → 选图片 → 客户端用 canvas 居中裁成正方形并缩放到 256×256，导出 WebP（不支持时 JPEG，质量 0.85）→ 显示预览 → 「使用这张」上传 / 「取消」。另有「移除头像」。所有操作有 busy / 错误状态。
- Tauri CSP（`app/src-tauri/tauri.conf.json`）的 `img-src` 加上 `blob:`，否则预览在客户端里显示不出来。
- 同阶段 P2-4 会给「文件传输助手」会话加专用图标，P2-1 改图片气泡，P2-3 改输入框和语音气泡；你只改头像相关的那几行，别动其他逻辑。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== V2 P2-2 =====`：

1. 上传 PNG 成功：`avatar_version` 从 0 变 1、`has_avatar=true`；`GET /users/{id}/avatar?v=1` 返回同样字节、缓存头为 immutable；旧版本号请求返回 `no-cache`。
2. 再次上传：版本 2，旧文件最终被删除（可轮询等待）。
3. 伪装类型：`image/png` 但内容是 HTML → 400；> 1MB → 400。
4. 删除头像：`has_avatar=false`，`GET` 404。
5. 未登录上传 401；不能改别人头像（接口本身只作用于自己）。
6. 好友的 `GET /friends`、会话列表 peer、消息的 `sender_avatar_version` 都反映新版本。
7. 头像变更后好友的 WS 收到 `invalidate`。

Vitest：裁剪尺寸计算函数（任意宽高 → 源裁剪矩形）；store 里头像版本合并函数（只增不减）。

浏览器实测（真实执行，截图放 `server/target/wt/`）：上传头像后，本人「我」页、侧栏、消息行更新；另一个账号的会话列表和聊天里看到新头像；移除后回到首字母。

## 不做

- 群头像、交互式裁剪框（拖动 / 缩放选区）。
- 头像审核。
