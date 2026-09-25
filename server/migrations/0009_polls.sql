-- 轻量投票（§3.6/§8.4）：polls 挂在 kind='poll' 的消息上（1:1）。
-- 选项创建后不可改，避免改题污染已投结果；投票是"替换本人选择集合"而非累加。
-- poll_votes 用 (poll_id, option_id) 复合外键证明选项归属同一 poll。

CREATE TABLE polls (
    message_id     uuid PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    question       text NOT NULL CHECK (char_length(question) BETWEEN 1 AND 200),
    allow_multiple boolean NOT NULL DEFAULT false,
    anonymous      boolean NOT NULL DEFAULT false,
    closed_at      timestamptz,
    expires_at     timestamptz,
    -- 投票级单调版本：投票/关闭等真实变更 +1；与 messages.edit_version 同步递增
    version        bigint NOT NULL DEFAULT 0
);

CREATE TABLE poll_options (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    poll_id      uuid NOT NULL REFERENCES polls(message_id) ON DELETE CASCADE,
    option_index integer NOT NULL,
    label        text NOT NULL CHECK (char_length(label) BETWEEN 1 AND 100),
    UNIQUE (poll_id, option_index),
    -- 供 poll_votes 复合外键引用 (poll_id, id)
    UNIQUE (poll_id, id)
);

CREATE TABLE poll_votes (
    poll_id    uuid NOT NULL REFERENCES polls(message_id) ON DELETE CASCADE,
    option_id  uuid NOT NULL,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (poll_id, option_id, user_id),
    FOREIGN KEY (poll_id, option_id) REFERENCES poll_options(poll_id, id) ON DELETE CASCADE
);
CREATE INDEX idx_poll_votes_user ON poll_votes(poll_id, user_id);

-- 统一消息读模型：追加 poll 列（只能在末尾加列）。
-- 聚合 option counts / total_voters / 各选项 voters；匿名掩蔽与 my_option_ids
-- 由 into_view(viewer) 逐人计算。墓碑消息掩去 poll（与正文/附件一致）。
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
    COALESCE(rx.reactions, '[]'::jsonb) AS reactions,
    CASE WHEN m.deleted_at IS NULL THEN pz.poll END AS poll
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
