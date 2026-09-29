-- Agent A27：工作区与工作区对话。
-- 工作区 = 某台电脑上的一个目录。移除只做软删除，再加同一路径时连同它下面的对话一起回来。
CREATE TABLE agent_workspaces (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    host_id    uuid NOT NULL REFERENCES agent_hosts(id) ON DELETE CASCADE,
    path       text NOT NULL CHECK (char_length(path) BETWEEN 1 AND 500),
    created_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz,
    UNIQUE (host_id, path)
);

-- 工作区对话：type='agent' 且 workspace_id 非空，一个 Agent 可以有任意多条。
-- 专属会话 workspace_id 为空，每个 Agent 仍恰好一个。
-- agent_model / agent_mode：对话里选的模型、模式，空串 = 跟随 Agent 配置。
ALTER TABLE conversations
    ADD COLUMN workspace_id uuid REFERENCES agent_workspaces(id) ON DELETE CASCADE,
    ADD COLUMN agent_model text NOT NULL DEFAULT '',
    ADD COLUMN agent_mode text NOT NULL DEFAULT '';

DROP INDEX uq_agent_conversation;
CREATE UNIQUE INDEX uq_agent_conversation
    ON conversations(agent_id) WHERE type = 'agent' AND workspace_id IS NULL;
CREATE INDEX idx_conversations_workspace
    ON conversations(workspace_id) WHERE workspace_id IS NOT NULL;

-- 已有 Agent 的工作目录直接作为工作区出现
INSERT INTO agent_workspaces (host_id, path)
SELECT DISTINCT host_id, cwd FROM agents WHERE cwd <> '' AND deleted_at IS NULL
ON CONFLICT DO NOTHING;
