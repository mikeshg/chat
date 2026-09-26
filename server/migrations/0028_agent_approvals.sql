-- Agent A12：权限档位 + 审批卡片。
-- agents.permission_tier：read_only / workspace / full，新建默认只读。
-- agent_approvals：一次超出档位的操作请求，卡片消息在 Agent 专属会话里。

ALTER TABLE agents
    ADD COLUMN permission_tier text NOT NULL DEFAULT 'read_only';

ALTER TABLE agents
    ADD CONSTRAINT agents_permission_tier_check
    CHECK (permission_tier IN ('read_only', 'workspace', 'full'));

CREATE TABLE agent_approvals (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    invocation_id uuid NOT NULL REFERENCES agent_invocations(id) ON DELETE CASCADE,
    message_id    uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- {title, kind, locations, request_id, invocation_id, tool_call}
    request       jsonb NOT NULL,
    -- [{optionId, name, kind}]
    options       jsonb NOT NULL,
    status        text NOT NULL CHECK (status IN ('pending', 'decided', 'expired')),
    decision      text,
    decided_at    timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX idx_agent_approvals_invocation ON agent_approvals(invocation_id);
CREATE INDEX idx_agent_approvals_message ON agent_approvals(message_id);

-- 统一消息读模型：只能在末尾追加列。以 0024 的视图为准，末尾加 approval。
CREATE OR REPLACE VIEW message_views AS
SELECT
    m.id,
    m.conversation_id,
    m.seq,
    m.sender_id,
    u.username::text AS sender_username,
    u.display_name AS sender_name,
    CASE WHEN m.deleted_at IS NULL THEN m.content ELSE '消息已删除' END AS content,
    m.kind,
    CASE WHEN m.deleted_at IS NULL THEN a.id END AS att_id,
    CASE WHEN m.deleted_at IS NULL THEN a.file_name END AS att_name,
    CASE WHEN m.deleted_at IS NULL THEN a.mime END AS att_mime,
    CASE WHEN m.deleted_at IS NULL THEN a.size END AS att_size,
    CASE WHEN m.deleted_at IS NULL THEN mmx.mentions END AS mentions,
    m.created_at,
    m.reply_to_id,
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
    CASE WHEN m.deleted_at IS NULL THEN a.width END AS att_width,
    CASE WHEN m.deleted_at IS NULL THEN a.height END AS att_height,
    CASE WHEN m.deleted_at IS NULL THEN a.thumb_path IS NOT NULL END AS att_has_thumb,
    CASE WHEN m.deleted_at IS NULL THEN a.duration_ms END AS att_duration_ms,
    u.avatar_version AS sender_avatar_version,
    u.avatar_path IS NOT NULL AS sender_has_avatar,
    m.forwarded_from,
    m.agent_id,
    ag.name AS agent_name,
    ag.avatar_version AS agent_avatar_version,
    CASE WHEN m.deleted_at IS NULL THEN m.agent_steps END AS agent_steps,
    CASE WHEN m.deleted_at IS NULL THEN ap.approval END AS approval
FROM messages m
JOIN users u ON u.id = m.sender_id
LEFT JOIN attachments a ON a.id = m.attachment_id AND m.deleted_at IS NULL
LEFT JOIN messages rm ON rm.id = m.reply_to_id
LEFT JOIN users ru ON ru.id = rm.sender_id
LEFT JOIN agents ag ON ag.id = m.agent_id
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
) pz ON true
LEFT JOIN LATERAL (
    SELECT jsonb_build_object(
        'id',       appr.id,
        'status',   appr.status,
        'request',  appr.request,
        'options',  appr.options,
        'decision', appr.decision
    ) AS approval
    FROM agent_approvals appr
    WHERE appr.message_id = m.id
) ap ON true;
