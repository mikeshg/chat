-- 消息生命周期：回复引用、编辑版本、软删除、emoji reactions。
-- edit_version 是消息级单调变更版本：编辑、软删除、reaction 实际变更均 +1。

ALTER TABLE messages
    ADD COLUMN reply_to_id  uuid REFERENCES messages(id) ON DELETE SET NULL,
    ADD COLUMN edited_at    timestamptz,
    ADD COLUMN edit_version integer NOT NULL DEFAULT 0,
    -- 最近一次成功编辑的 client_op_id：同 op 重试直接返回现状，不误报版本冲突
    ADD COLUMN edit_op_id   text,
    ADD COLUMN deleted_at   timestamptz,
    ADD COLUMN deleted_by   uuid REFERENCES users(id) ON DELETE SET NULL;

CREATE INDEX idx_messages_reply_to ON messages(reply_to_id) WHERE reply_to_id IS NOT NULL;

CREATE TABLE message_reactions (
    message_id uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- emoji 串最大 32 字节（按字节数，不是字符数）
    emoji      text NOT NULL CHECK (octet_length(emoji) BETWEEN 1 AND 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (message_id, user_id, emoji)
);

-- 统一消息读模型：追加生命周期字段；软删除消息掩去正文/附件/提及，保留 id/seq/sender/时间。
CREATE OR REPLACE VIEW message_views AS
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
    COALESCE(rx.reactions, '[]'::jsonb) AS reactions
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
) rx ON true;
