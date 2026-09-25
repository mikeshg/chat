-- 发送幂等键、附件只能被一条消息引用、解散保留消息、统一消息读模型。

ALTER TABLE messages ADD COLUMN client_msg_id text;
ALTER TABLE messages ADD CONSTRAINT messages_client_msg_len
    CHECK (client_msg_id IS NULL OR char_length(client_msg_id) BETWEEN 1 AND 80);

CREATE UNIQUE INDEX uq_messages_client_msg
    ON messages (conversation_id, sender_id, client_msg_id)
    WHERE client_msg_id IS NOT NULL;

-- 拆分元数据保留每条历史引用；物理文件仍共享，删除时按 storage_path 检查剩余引用。
WITH duplicate_refs AS MATERIALIZED (
    SELECT m.id AS message_id, m.attachment_id, gen_random_uuid() AS new_attachment_id
    FROM messages m
    WHERE m.attachment_id IS NOT NULL
      AND EXISTS (
          SELECT 1 FROM messages m2
          WHERE m2.attachment_id = m.attachment_id
            AND (m2.created_at, m2.id) < (m.created_at, m.id)
      )
), cloned AS (
    INSERT INTO attachments (id, uploader_id, conv_id, file_name, mime, size, storage_path, created_at)
    SELECT d.new_attachment_id, a.uploader_id, a.conv_id, a.file_name, a.mime, a.size, a.storage_path, a.created_at
    FROM duplicate_refs d
    JOIN attachments a ON a.id = d.attachment_id
    RETURNING id
)
UPDATE messages m
SET attachment_id = d.new_attachment_id
FROM duplicate_refs d
JOIN cloned c ON c.id = d.new_attachment_id
WHERE m.id = d.message_id;

CREATE UNIQUE INDEX uq_messages_attachment
    ON messages (attachment_id)
    WHERE attachment_id IS NOT NULL;

-- 群主解散：成员全部移出，会话与消息保留。最后一人退群仍删除会话。
ALTER TABLE conversations ADD COLUMN dissolved_at timestamptz;

CREATE VIEW message_views AS
SELECT
    m.id,
    m.conversation_id,
    m.seq,
    m.sender_id,
    u.username::text AS sender_username,
    u.display_name AS sender_name,
    m.content,
    m.kind,
    a.id AS att_id,
    a.file_name AS att_name,
    a.mime AS att_mime,
    a.size AS att_size,
    mmx.mentions,
    m.created_at
FROM messages m
JOIN users u ON u.id = m.sender_id
LEFT JOIN attachments a ON a.id = m.attachment_id
LEFT JOIN LATERAL (
    SELECT array_agg(user_id) AS mentions
    FROM message_mentions
    WHERE message_id = m.id
) mmx ON true;
