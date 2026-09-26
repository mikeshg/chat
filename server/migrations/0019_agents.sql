-- Agent A01：AgentHost / Agent / AgentConversation / AgentInvocation 最小端到端。
-- 每个 Agent 固定在一台宿主（AgentHost）上，拥有一个专属会话（AgentConversation）。

-- 宿主：桌面端安装。install_id 是桌面端生成的永久标识，重新登录/改密不变；
-- 同一台电脑换用户登录是 (user_id, install_id) 不同的另一条记录。
CREATE TABLE agent_hosts (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    install_id   uuid NOT NULL,
    label        text NOT NULL DEFAULT '' CHECK (char_length(label) <= 60),
    platform     text NOT NULL DEFAULT '',
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, install_id)
);
CREATE INDEX idx_agent_hosts_user ON agent_hosts(user_id);

-- Agent：主人名下名字不区分大小写唯一；runtime 为 jsonb 快照
-- （A01 只有 {"command": "...", "args": ["..."]} 一种形状）。
CREATE TABLE agents (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id   uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    host_id    uuid NOT NULL REFERENCES agent_hosts(id) ON DELETE CASCADE,
    name       text NOT NULL CHECK (name ~ '^[A-Za-z0-9_][A-Za-z0-9_-]{0,31}$'),
    runtime    jsonb NOT NULL,
    -- 宿主机上的工作目录；空串 = 由宿主决定
    cwd        text NOT NULL DEFAULT '',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX uq_agents_owner_name ON agents(owner_id, lower(name));

-- AgentConversation：type='agent'，agent_id 指向所属 Agent，每个 Agent 恰好一个。
-- owner_id = 主人，唯一成员（memberships role='owner'），name 恒 NULL。
ALTER TABLE conversations DROP CONSTRAINT conversations_type_check;
ALTER TABLE conversations ADD CONSTRAINT conversations_type_check
    CHECK (type IN ('dm', 'group', 'self', 'agent'));
ALTER TABLE conversations
    ADD COLUMN agent_id uuid REFERENCES agents(id) ON DELETE CASCADE;
CREATE UNIQUE INDEX uq_agent_conversation
    ON conversations(agent_id) WHERE type = 'agent';

-- Agent 发出的消息：agent_id 非空；sender_id 仍填主人（保持 NOT NULL 与连表不变）。
ALTER TABLE messages
    ADD COLUMN agent_id uuid REFERENCES agents(id);

-- 调用记录：A01 只用 running / succeeded / failed；
-- queued / awaiting_approval / cancelled / timed_out 为后续票预留（CHECK 一次写全）。
CREATE TABLE agent_invocations (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id           uuid NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    conversation_id    uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    trigger_message_id uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    status             text NOT NULL CHECK (status IN
        ('queued','running','awaiting_approval','succeeded','failed','cancelled','timed_out')),
    error              text,
    created_at         timestamptz NOT NULL DEFAULT now(),
    started_at         timestamptz,
    finished_at        timestamptz
);
CREATE INDEX idx_agent_invocations_conv
    ON agent_invocations(conversation_id, created_at DESC);
CREATE INDEX idx_agent_invocations_running
    ON agent_invocations(agent_id) WHERE finished_at IS NULL;

-- 统一消息读模型：只能在末尾追加列（本次加 agent_id / agent_name）
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
    CASE WHEN m.deleted_at IS NULL THEN pz.poll END AS poll,
    -- V2 第 2 阶段（0012）：图片尺寸 / 缩略图 / 语音时长（墓碑掩去）、发送者头像、转发来源
    CASE WHEN m.deleted_at IS NULL THEN a.width END AS att_width,
    CASE WHEN m.deleted_at IS NULL THEN a.height END AS att_height,
    CASE WHEN m.deleted_at IS NULL THEN a.thumb_path IS NOT NULL END AS att_has_thumb,
    CASE WHEN m.deleted_at IS NULL THEN a.duration_ms END AS att_duration_ms,
    u.avatar_version AS sender_avatar_version,
    u.avatar_path IS NOT NULL AS sender_has_avatar,
    m.forwarded_from,
    -- Agent A01（0019）：非空 = 该消息由 Agent 发出（sender_id 仍是主人）
    m.agent_id,
    ag.name AS agent_name
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
