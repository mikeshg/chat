# P2-3 语音消息

先读 [../devin-rules.md](../devin-rules.md) 和 [P2-0-media-schema.md](P2-0-media-schema.md)（`attachments.duration_ms` 与视图列 `att_duration_ms`、`AttachmentView.duration_ms` 已由主会话建好，**不要再改 `message_views`**）。迁移号：不需要。

## 背景

手机上打字慢，熟人聊天里语音消息是刚需。现有附件链路：`POST /v1/files`（multipart，`conversation_id` + `file`）→ WS `send_message { attachment_id }`；消息 `kind` 由服务端按附件 MIME 推断（`domain.rs` `normalize_body`：`image/*` → `image`，其余 → `file`）。

## 规则

- 语音 = 带 `duration_ms` 的音频附件。上传时 multipart 多一个可选字段 `duration_ms`：
  - 只有 MIME 以 `audio/` 开头时才接受，否则 400。
  - 范围 1–300,000（5 分钟），否则 400。
  - 写入 `attachments.duration_ms`。
- 发送时附件有 `duration_ms` → 消息 `kind = 'audio'`，正文为空时占位 `[语音]`（旧客户端看到这段文字）。普通音频文件（没有 `duration_ms`）仍是 `file`，不变。
- 语音消息不可编辑（现有「只有文本可编辑」规则已覆盖，确认一下）。
- `AttachmentView.duration_ms` 在上传响应和消息视图里都返回。

## 客户端

### 录音

- 输入框工具栏加麦克风按钮（新组件 `app/src/ui/VoiceRecorder.tsx`，`ChatView.tsx` 只加挂载和发送回调）。
- 点击开始录音，进入录音条：红点 + 计时 `0:07` + 「取消」「发送」。到 120 秒自动停止并发送。
- 录音格式按顺序挑第一个 `MediaRecorder.isTypeSupported` 的：`audio/webm;codecs=opus` → `audio/ogg;codecs=opus` → `audio/mp4`。时长用录音计时（毫秒），不从文件读（Chrome 录的 webm 没有时长元数据）。
- 短于 1 秒不发送，提示「说话时间太短」。
- 发送：`files.upload(conversationId, file, { durationMs })` → 现有发送链路（附件消息、outbox、失败重试沿用）。文件名 `voice-<时间戳>.webm`（按实际格式取扩展名）。
- 录音结束 / 取消 / 切会话 / 组件卸载时，一定要 `track.stop()` 释放麦克风。
- 不支持的环境：`navigator.mediaDevices?.getUserMedia` 不存在（HTTP 非安全上下文、老浏览器）时按钮禁用，悬停提示「需要 HTTPS 或 localhost 才能录音」。权限被拒时提示「麦克风权限被拒绝」。

### 播放

- 新组件 `app/src/ui/AudioBubble.tsx`：播放 / 暂停按钮 + 时长 `12"` + 进度条；气泡宽度随时长增长（最短约 80px，最长约 220px，按时长线性插值后封顶）。
- 同一时间只播放一条：开始播放新的会暂停其他的（模块级单例即可）。
- 播放地址用现有 `files.url(id)`。
- 会话列表预览、toast、通知摘要里语音显示 `[语音]`（找到 `kind` → 摘要文字的地方补上 `audio`，比如 `ChatView.tsx` 的 `kindLabel`、`ws.ts` 的 toast 摘要、第 1 阶段 `notify.ts` 的正文函数）。

### 平台

- Tauri CSP（`app/src-tauri/tauri.conf.json`）：`media-src` 加上 `blob:`（录音试听 / 播放本地 blob 时需要）。
- Android：`app/src-tauri/gen/android/app/src/main/AndroidManifest.xml` 加 `android.permission.RECORD_AUDIO` 和 `android.permission.MODIFY_AUDIO_SETTINGS`。查看 `gen/android` 里 WebChromeClient 的 `onPermissionRequest` 实现（wry 生成的代码），确认网页的麦克风请求会触发系统运行时权限；如果需要改生成代码才能生效，改动写清楚放进交付文档，**不要**改 wry 依赖本身。
- Windows（WebView2）：记录实际表现（是否弹权限框）；不强求自动化验证。

## 测试

`server/tests/it.rs` 末尾分节 `// ===== V2 P2-3 =====`：

1. 上传 `audio/webm` + `duration_ms=5300` → 响应带 `duration_ms`；发送后消息 `kind='audio'`、正文 `[语音]`、`attachment.duration_ms=5300`；历史接口、WS 广播、sync 里一致。
2. `duration_ms` 配 `image/png` → 400；`duration_ms=0`、`300001`、非数字 → 400。
3. 不带 `duration_ms` 的 `audio/mpeg` → 仍是 `kind='file'`。
4. 语音消息编辑被拒；删除后 `duration_ms` 随墓碑一起掩去。

Vitest：录音格式挑选函数（传入 `isTypeSupported` 桩）；气泡宽度函数；时长格式化（`0:07`、`12"`）；过短判定。

浏览器实测（真实执行）：Playwright 启动 Chromium 时加 `--use-fake-ui-for-media-stream --use-fake-device-for-media-stream`，并 `grantPermissions(["microphone"])`，录 3 秒 → 发送 → 对端看到语音气泡、时长约 3 秒、能播放（断言 `audio` 元素 `paused === false`）。截图放 `server/target/wt/`。

## 不做

- 语音转文字、变速播放、按住说话的手势交互（移动端长按）、录音波形图。
