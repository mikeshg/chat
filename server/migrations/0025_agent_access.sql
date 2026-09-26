-- Agent A08：谁能使用 Agent。
-- server_settings 仍是 key → jsonb，不改成列（与注册模式、配额同一套存取）。
-- 缺键时代码按 true 处理，这里预置一行，效果等同 NOT NULL DEFAULT true。
-- users.agent_access 为 NULL 表示跟随 agent_default_allowed。

ALTER TABLE users ADD COLUMN agent_access boolean;

INSERT INTO server_settings (key, value)
VALUES ('agent_enabled', 'true'::jsonb),
       ('agent_default_allowed', 'true'::jsonb)
ON CONFLICT (key) DO NOTHING;
