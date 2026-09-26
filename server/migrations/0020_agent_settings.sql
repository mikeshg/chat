-- Agent 第 2 批前的预重构：个人默认设置 + 单个 Agent 覆盖项。
-- 输出方式（A03）与排队超时（A04）共用这一层，避免并行票各建一套。

-- 个人默认：没有行 = 全部取默认值
CREATE TABLE agent_user_settings (
    user_id            uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    output_mode        text NOT NULL DEFAULT 'stream'
        CHECK (output_mode IN ('stream', 'status', 'steps')),
    -- 排队超时秒数；0 = 永不超时
    queue_timeout_secs integer NOT NULL DEFAULT 86400 CHECK (queue_timeout_secs >= 0),
    updated_at         timestamptz NOT NULL DEFAULT now()
);

-- 单个 Agent 覆盖：NULL = 跟随主人的个人默认
ALTER TABLE agents
    ADD COLUMN output_mode text CHECK (output_mode IN ('stream', 'status', 'steps')),
    ADD COLUMN queue_timeout_secs integer CHECK (queue_timeout_secs >= 0);
