-- 消息撤回（MM1）：删除改名撤回，24 小时内本人可撤回，群主可撤群内任意消息。
-- 撤回不再删除附件行/物理文件：attachments.recalled_at 标记隐藏（公开下载 404），
-- 上传者一天内经 GET /files/{id}/recalled 取回、按 reuse_attachment_id 重发；
-- 后台任务定期清理过期行，物理文件按 storage_path/thumb_path 引用计数删除。
ALTER TABLE attachments ADD COLUMN recalled_at timestamptz;
CREATE INDEX idx_attachments_recalled ON attachments(recalled_at) WHERE recalled_at IS NOT NULL;

-- 统一消息读模型：只能在末尾追加列。
-- 变化：墓碑占位文本 '消息已删除' → '消息已撤回'；reply_to 预览追加 recalled_by_name；
-- 末尾追加 recalled_by（= deleted_by）与 recalled_by_name（撤回者展示名，可被本人撤回或群主撤回）。
CREATE OR REPLACE VIEW message_views AS
SELECT
    m.id,
    m.conversation_id,
    m.seq,
    m.sender_id,
    u.username::text AS sender_username,
    u.display_name AS sender_name,
    -- 墓碑占位文本：旧客户端没有 deleted_at 概念也能显示“消息已撤回”
    CASE WHEN m.deleted_at IS NULL THEN m.content ELSE '消息已撤回' END AS content,
    m.kind,
    CASE WHEN m.deleted_at IS NULL THEN a.id END AS att_id,
    CASE WHEN m.deleted_at IS NULL THEN a.file_name END AS att_name,
    CASE WHEN m.deleted_at IS NULL THEN a.mime END AS att_mime,
    CASE WHEN m.deleted_at IS NULL THEN a.size END AS att_size,
    CASE WHEN m.deleted_at IS NULL THEN mmx.mentions END AS mentions,
    m.created_at,
    m.reply_to_id,
    -- 回复预览只展开一层；原消息撤回后 excerpt 清空、deleted=true，仍带 id/seq 供定位
    CASE WHEN m.reply_to_id IS NULL THEN NULL ELSE jsonb_build_object(
        'message_id', rm.id,
        'seq', rm.seq,
        'sender_name', ru.display_name,
        'excerpt', CASE WHEN rm.deleted_at IS NULL THEN left(rm.content, 120) ELSE '' END,
        'kind', rm.kind,
        'deleted', rm.deleted_at IS NOT NULL,
        'recalled_by_name', rdu.display_name
    ) END AS reply_to,
    m.edited_at,
    m.edit_version,
    m.deleted_at,
    COALESCE(rx.reactions, '[]'::jsonb) AS reactions,
    CASE WHEN m.deleted_at IS NULL THEN pz.poll END AS poll,
    -- V2 第 2 阶段（0012）：图片尺寸 / 缩略图 / 语音时长（墓碑掩去）、发送者头像、转发来源
    CASE WHEN m.deleted_at IS NULL THEN a.width END AS att_width,
    CASE WHEN m.deleted_at IS NULL THEN a.height END AS att_height,
    CASE WHEN m.deleted_at IS NULL THEN a.thumb_path IS NOT NULL END AS att_has_thumb,
    CASE WHEN m.deleted_at IS NULL THEN a.duration_ms END AS att_duration_ms,
    u.avatar_version AS sender_avatar_version,
    u.avatar_path IS NOT NULL AS sender_has_avatar,
    m.forwarded_from,
    -- 撤回（0039）：recalled = deleted_at IS NOT NULL 由读模型派生；撤回者与展示名
    m.deleted_by AS recalled_by,
    du.display_name AS recalled_by_name
FROM messages m
JOIN users u ON u.id = m.sender_id
LEFT JOIN attachments a ON a.id = m.attachment_id AND m.deleted_at IS NULL
LEFT JOIN users du ON du.id = m.deleted_by
LEFT JOIN messages rm ON rm.id = m.reply_to_id
LEFT JOIN users ru ON ru.id = rm.sender_id
LEFT JOIN users rdu ON rdu.id = rm.deleted_by
LEFT JOIN LATERAL (
    SELECT array_agg(user_id) AS mentions
    FROM message_mentions
    WHERE message_id = m.id
) mmx ON true
LEFT JOIN LATERAL (
    SELECT jsonb_agg(jsonb_build_object('emoji', t.emoji, 'users', t.users)) AS reactions
    FROM (
        SELECT emoji, array_agg(user_id) AS users
        FROM message_reactions
        WHERE message_id = m.id
        GROUP BY emoji
    ) t
) rx ON true
LEFT JOIN LATERAL (
    SELECT jsonb_build_object(
        'question',       p.question,
        'allow_multiple', p.allow_multiple,
        'anonymous',      p.anonymous,
        'closed_at',      p.closed_at,
        'expires_at',     p.expires_at,
        -- 到期态按数据库 now() 计算（读取与写入同口径；客户端时钟不可信）
        'expired',        (p.expires_at IS NOT NULL AND p.expires_at <= now()),
        'version',        p.version,
        'total_voters',   COALESCE(tv.n, 0),
        'options',        COALESCE(opts.items, '[]'::jsonb)
    ) AS poll
    FROM polls p
    LEFT JOIN LATERAL (
        SELECT count(DISTINCT v.user_id) AS n
        FROM poll_votes v
        WHERE v.poll_id = p.message_id
    ) tv ON true
    LEFT JOIN LATERAL (
        SELECT jsonb_agg(
            jsonb_build_object(
                'id',     o.id,
                'index',  o.option_index,
                'label',  o.label,
                'count',  COALESCE(oc.cnt, 0),
                'voters', COALESCE(oc.voters, '[]'::jsonb)
            ) ORDER BY o.option_index
        ) AS items
        FROM poll_options o
        LEFT JOIN LATERAL (
            SELECT count(*) AS cnt, jsonb_agg(v.user_id) AS voters
            FROM poll_votes v
            WHERE v.poll_id = o.poll_id AND v.option_id = o.id
        ) oc ON true
        WHERE o.poll_id = p.message_id
    ) opts ON true
    WHERE p.message_id = m.id
) pz ON true;
