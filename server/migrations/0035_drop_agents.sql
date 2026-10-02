-- 去掉 Agent：纯聊天 + webhook bot（见 0036）。
-- Agent 专属会话、工作区对话整个删除；Agent 在私聊 / 群里发过的消息硬删
-- （否则 sender_id 是主人，会显示成主人本人发的）。seq 因此出现空洞，客户端按 seq 排序和补齐，不受影响。
-- Agent 产出的附件行一并删除（不再占主人的存储配额）；磁盘上的文件不清理。
-- ponytail: 孤儿附件文件留在 FILES_DIR，需要时按 attachments.storage_path 对账清理。

DROP VIEW message_views;

DELETE FROM messages WHERE agent_id IS NOT NULL;
DELETE FROM conversations WHERE type = 'agent';
DELETE FROM attachments WHERE agent_invocation_id IS NOT NULL;

DROP TABLE agent_approvals, agent_contexts, agent_schedules, agent_invocations,
           agent_user_settings CASCADE;
ALTER TABLE messages DROP COLUMN agent_id, DROP COLUMN agent_steps;
ALTER TABLE attachments DROP COLUMN agent_invocation_id;
ALTER TABLE conversations
    DROP COLUMN agent_id,
    DROP COLUMN workspace_id,
    DROP COLUMN agent_model,
    DROP COLUMN agent_mode;
ALTER TABLE users DROP COLUMN agent_access;
DROP TABLE agent_workspaces, agents, agent_hosts CASCADE;
DROP FUNCTION IF EXISTS agent_invocations_origin();

ALTER TABLE conversations DROP CONSTRAINT conversations_type_check;
ALTER TABLE conversations ADD CONSTRAINT conversations_type_check
    CHECK (type IN ('dm', 'group', 'self'));

DELETE FROM server_settings
 WHERE key IN ('agent_enabled', 'agent_default_allowed', 'builtin_endpoints');

-- 统一消息读模型：0019 之前的列，去掉 agent_id / agent_name 等
CREATE VIEW message_views AS
SELECT
    m.id,
    m.conversation_id,
    m.seq,
    m.sender_id,
    u.username::text AS sender_username,
    u.display_name AS sender_name,
    -- 墓碑占位文本：旧客户端没有 deleted_at 概念也能显示“消息已删除”
    CASE WHEN m.deleted_at IS NULL THEN m.content ELSE '消息已删除' END AS content,
    m.kind,
    CASE WHEN m.deleted_at IS NULL THEN a.id END AS att_id,
    CASE WHEN m.deleted_at IS NULL THEN a.file_name END AS att_name,
    CASE WHEN m.deleted_at IS NULL THEN a.mime END AS att_mime,
    CASE WHEN m.deleted_at IS NULL THEN a.size END AS att_size,
    CASE WHEN m.deleted_at IS NULL THEN mmx.mentions END AS mentions,
    m.created_at,
    m.reply_to_id,
    -- 回复预览只展开一层；原消息删除后 excerpt 清空、deleted=true，仍带 id/seq 供定位
    CASE WHEN m.reply_to_id IS NULL THEN NULL ELSE jsonb_build_object(
        'message_id', rm.id,
        'seq', rm.seq,
        'sender_name', ru.display_name,
        'excerpt', CASE WHEN rm.deleted_at IS NULL THEN left(rm.content, 120) ELSE '' END,
        'kind', rm.kind,
        'deleted', rm.deleted_at IS NOT NULL
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
    m.forwarded_from
FROM messages m
JOIN users u ON u.id = m.sender_id
LEFT JOIN attachments a ON a.id = m.attachment_id AND m.deleted_at IS NULL
LEFT JOIN messages rm ON rm.id = m.reply_to_id
LEFT JOIN users ru ON ru.id = rm.sender_id
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
