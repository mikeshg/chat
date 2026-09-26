-- Agent A02：AgentContext。同一 Agent 在某个上下文键下记住运行时会话 id。
-- context_key 用 text：本票是发起调用的会话 id；以后调用方可自选上下文号，所以不做成 uuid 外键。

CREATE TABLE agent_contexts (
    agent_id           uuid NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    context_key        text NOT NULL,
    runtime_session_id text NOT NULL,
    updated_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, context_key)
);
