# 第 3、4 阶段设计草稿（第 2 阶段合并后再写正式任务单）

## P3-1 全局搜索 + 文件列表（0016）
- 0016：`CREATE EXTENSION IF NOT EXISTS pg_trgm`；`gin (content gin_trgm_ops) WHERE deleted_at IS NULL`
- 一个接口 `GET /v1/search?q=&conversation_id=&sender_id=&kind=&from=&to=&before=&limit=`：q 可空（有筛选时=浏览文件），ILIKE 复用现有转义；只查当前成员会话；按 created_at DESC,id；返回 MessageView 列表（客户端按会话目录拼标题）
- 会话文件列表 = 同一接口带 conversation_id + kind
- 客户端：ConvList 顶部搜索框（会话标题本地过滤 + 服务端聊天记录）；类型筛选、发送者、日期（原生 date input）；点结果 → store.pendingJump → ChatView 打开并定位（复用 jumpToRef）；GroupInfo / 私聊信息里「文件与图片」页签（缩略图网格 + 文件列表，before 游标分页）

## P3-2 群公告 + 置顶消息 + 会议链接（0017）
- conversations.announcement/announcement_by/announcement_at（≤2000）；仅群主改；DM/self 无公告
- conversation_pins(conversation_id, message_id, pinned_by, pinned_at) PK 两列，每会话 ≤20；任何成员可置顶/取消；墓碑过滤
- ConversationView.announcement {text, updated_by_name, updated_at}；变更 notify invalidate；WS `pins_updated {conversation_id}`
- 头部：公告条（折叠/展开，群主可编辑，新公告高亮 localStorage 记已读时间）+ 置顶条（最新一条 + 列表面板，点跳转）；消息菜单「置顶/取消置顶」
- D2：server_settings.meeting_base_url 默认 https://meet.jit.si，server-info 暴露；管理后台可改（http/https，≤200）；头部 📹 → 确认 → 发文本 `📹 发起了视频会议，点击加入：{base}/chat-<12位随机>`（依赖 P1-4 链接渲染）；DM 与群可用

## P3-3 ntfy 离线推送 + 心跳（0018）
- 防 SSRF：推送服务器地址只由管理员配置（server_settings push_ntfy_base、push_ntfy_token 只写不回显、public_url 用于 Click 跳转）；用户只拿到服务端生成的随机 topic
- 表 push_settings(user_id PK, enabled, topic, preview default false, updated_at)
- 触发点：P2-4 的 ws::broadcast_posted；成员 ≠ 发送者、无「活跃」连接、推送开启、未静音（@我 突破）、归档会话只推 @我
- 活跃：WS 入站帧 `client_state {active}`（visibilitychange/focus/blur + 连接时上报），连接默认 active=true 兼容旧客户端；推送条件 = 该用户没有 active 连接
- 合并：同 (user, conv) 30s 内只推一次（DashMap<(u,c), Instant>，ponytail 注释）
- 发送：reqwest（改为正式依赖，rustls）POST base/topic，头 Title/Priority(@我 high)/Tags/Click，Bearer token；5s 超时、失败只记日志
- 心跳：handle_session 记录最近入站时间，ping tick 时超过 75s 无入站就关闭；AppState 放一个可调的 AtomicU64 超时供测试缩短
- 客户端 PushSettings.tsx：未配置显示提示；开启后显示订阅地址 + 复制 + 说明；预览开关；测试推送；重置主题
- 管理后台：ntfy 地址 / token（显示「已设置」）/ public_url
- 测试：假 ntfy 服务（测试内 axum 监听）记录请求；全部 spawn_named_db

## P4-1 备份 / 恢复
- 格式：tar.gz（tar + flate2 依赖），内含 manifest.json（格式版本、最新迁移版本号、导出时间、表清单与行数）、db/<table>.csv（`COPY ... TO STDOUT WITH (FORMAT csv, HEADER)`，经 sqlx copy_out_raw）、files/…（FILES_DIR 全量，含 avatars、thumbs）
- 表清单：从 information_schema 动态读 public 下所有基表（排除 _sqlx_migrations），按外键拓扑排序；恢复时只接受 manifest 迁移版本 == 当前 _sqlx_migrations 最大版本
- 生成：POST /admin/backups → 后台任务写到 FILES_DIR/../backups 或独立目录 → 列表 GET /admin/backups（文件名、大小、时间）→ GET /admin/backups/{name} 下载（admin token 用短时 ticket，因为 <a> 带不了头）→ DELETE
- 恢复：POST /admin/restore（multipart 上传，body 上限单独放大，流式落盘）→ 校验 manifest → 进入维护模式（AtomicBool，中间件对除 /admin/restore 状态外所有请求 503）→ 单事务 TRUNCATE 全部表 + 按拓扑顺序 COPY FROM STDIN → 文件目录：解到新目录后原子替换 → 踢全部连接 force_logout(reason restored) → 退出维护模式
- 二次确认：请求体带 confirm="RESTORE"；UI 输入框要求键入确认词
- 测试：独立库；备份→改数据→恢复→数据回到备份时刻（含附件字节、头像）；版本不符拒绝；非管理员 403；维护期间其他请求 503

## P4-2 E2E + 文档
- scripts/e2e.mjs 增加 V2 主路径（注册模式/邀请、改密/设备、转发、self、语音上传、缩略图、搜索、公告/置顶、推送假服务可选）
- README 功能列表、CONTEXT.md 词汇（Session、Invite、SelfConversation、Pin、Announcement、PushTopic、Backup）
