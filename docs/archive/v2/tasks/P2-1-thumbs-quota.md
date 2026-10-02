# P2-1 图片缩略图 + 每用户存储配额

先读 [../devin-rules.md](../devin-rules.md) 和 [P2-0-media-schema.md](P2-0-media-schema.md)（`attachments.width/height/thumb_path` 与视图列已由主会话建好，**不要再改 `message_views`**）。迁移号：**`0013`**（仅当确实需要加索引时使用，否则不建；建了要追加到 `server/tests/migrations.rs` 的迁移列表末尾）。

## 背景

- 图片气泡直接加载原图（`GET /v1/files/{id}`），一张 10MB 照片在列表里也全量下载；图片尺寸未知，加载完成前后气泡高度跳动，滚动位置会乱。
- 附件无容量控制，`FILES_DIR` 会无限增长。
- 第 1 阶段已有 `server_settings`（`server/src/settings.rs`）和管理后台设置组件 `AdminSettings.tsx`，本任务往里加一个键。

## 一、缩略图

### 规则

- 上传时 MIME 为 `image/jpeg`、`image/png`、`image/webp` 的文件，在 `tokio::task::spawn_blocking` 里解码：
  - 解码限制：`image::Limits` 设最大宽高 12,000、最大分配 512MB；超限或解码失败只记日志，附件照常入库（宽高、缩略图为 NULL）。
  - 按 EXIF 方向纠正后记录 `width` / `height`。
  - 最长边 > 480px 时生成缩略图：等比缩到最长边 480，JPEG 质量 80，存为 `<附件存储名>.thumb.jpg`，写 `thumb_path`。≤ 480px 不生成（直接用原图）。
- `image/gif` 只记录宽高（取第一帧），不生成缩略图（保留动图）。
- `svg` 等其他类型不处理。
- 上传响应 `AttachmentView` 带回 `width` / `height` / `has_thumb`。

### 接口

- `GET /v1/files/{id}?thumb=1`：有缩略图返回缩略图（`image/jpeg`，缓存头与原图一致），没有就返回原图。`dl=1` 时忽略 `thumb`。

### 删除与共享

- 现有软删除逻辑（`domain.rs` `delete_in_tx` 附近）只在「没有其他 attachments 行共享同一 `storage_path`」时才清理物理文件。清理原文件时，把同一行的 `thumb_path` 文件一起删掉。
- 同阶段 P2-4（转发）会复制 attachments 行、共享 `storage_path` 和 `thumb_path`，上面的引用计数规则保证不会误删。

## 二、存储配额

### 规则

- `server_settings` 新键 `storage_quota_mb`：整数，每个用户的附件总量上限；缺省或 0 表示不限。
- 用户已用量 = 该用户上传（`uploader_id`）的附件按 `storage_path` 去重后的 `size` 之和（转发产生的副本共享存储，不重复计）。
- 上传时：已用量 + 本次大小 > 配额 → 413，消息 `存储空间已满（已用 X MB / 上限 Y MB）`。先按 `Content-Length` 或读取中累计判断，能早拒就早拒，但以实际写入字节为准。
- 并发上传可能略微超出配额，接受；在代码里用 `// ponytail:` 注释写明上限与升级路径（按用户加锁）。
- 管理员不受配额限制？**否**，一视同仁（管理员可以把配额调大）。

### 接口

- `GET /v1/users/me/storage` → `{ used_bytes, quota_bytes }`（不限时 `quota_bytes` 为 null）。
- `GET/PUT /v1/admin/settings` 增加 `storage_quota_mb`（0–1,048,576 的整数，非法 400）。
- `GET /v1/admin/users` 每行增加 `storage_bytes`。
- `GET /v1/server-info` **不**暴露配额。

## 客户端

- 图片气泡：`has_thumb` 时用 `?thumb=1` 地址；有 `width/height` 时按比例设置气泡图片尺寸（最长边 240px 左右，和现有样式一致），加载前就占好位置，避免跳动；点开预览仍加载原图。
- 「我」页：独立组件 `StorageUsage.tsx`（在 `MePanel.tsx` 加一行挂载），显示「已用 X / 上限 Y」和进度条；不限时显示「已用 X（不限额）」。
- 管理后台：`AdminSettings.tsx` 增加「每人存储上限（MB，0 = 不限）」输入与保存；用户列表增加「存储」列。
- 上传被 413 拒绝时，composer 的附件条上显示服务端返回的消息。

## 允许新增的依赖

`server/Cargo.toml`：`image = { version = "0.25", default-features = false, features = ["jpeg", "png", "webp", "gif"] }`。同阶段 P2-2 不需要 `image`。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== V2 P2-1 =====`：

1. 上传一张 1200×800 的 PNG（测试里用 `image` crate 现场生成，或放一张小的测试夹具到 `server/tests/fixtures/`）：响应 `width=1200,height=800,has_thumb=true`；`?thumb=1` 返回 JPEG 且最长边 480；消息视图里字段一致。
2. 300×200 的图片：有宽高、`has_thumb=false`，`?thumb=1` 返回原图。
3. 坏图片（`image/png` 但内容是随机字节）：上传成功、宽高为 null。
4. 删除消息后：原文件和缩略图都被清理；共享同一 `storage_path` 的另一行仍存在时两者都保留（可直接用 SQL 复制一行 attachments 模拟转发）。
5. 配额（**用 `spawn_named_db`**）：设 1MB，上传 600KB 成功、再传 600KB 返回 413；`/users/me/storage` 数值正确；复制出的共享行不重复计数；设为 0 后不限。
6. 非管理员设置配额 403；非法值 400。

Vitest：图片显示尺寸计算函数（原始宽高 → 气泡宽高）、存储用量格式化函数。

浏览器实测（真实执行，截图放 `server/target/wt/`）：发一张大图，气泡加载的是缩略图（网络面板或 Playwright 请求记录里能看到 `thumb=1`），点开是原图；管理员设 1MB 配额后上传大文件看到提示。

## 不做

- 视频缩略图、PDF 预览图。
- 历史图片的缩略图补生成（旧附件没有宽高和缩略图，按原图显示即可）。
- 过期文件自动清理。
