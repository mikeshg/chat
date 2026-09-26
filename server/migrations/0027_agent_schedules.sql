-- Agent A17：定时调用。每个主人最多 50 条由应用层拒绝（400 too_many）。
--
-- agent_invocations 必须有触发消息或定时任务二者之一。
-- 字面 CHECK 与 schedule_id ON DELETE SET NULL 冲突：定时调用没有 trigger_message_id，
-- 删除时间表时外键把 schedule_id 置空会违反 CHECK，删除失败、历史调用也删不掉。
-- 触发器实现同一条不变量，并放行「删除时间表导致的置空」。

CREATE TABLE agent_schedules (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id               uuid NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    owner_id               uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name                   text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 40),
    prompt                 text NOT NULL CHECK (char_length(prompt) BETWEEN 1 AND 4000),
    target_conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    spec                   jsonb NOT NULL,
    timezone               text NOT NULL CHECK (char_length(timezone) BETWEEN 1 AND 64),
    enabled                boolean NOT NULL DEFAULT true,
    next_run_at            timestamptz,
    last_run_at            timestamptz,
    created_by             text NOT NULL CHECK (created_by IN ('user', 'agent')),
    created_at             timestamptz NOT NULL DEFAULT now(),
    -- 停用原因：owner_disabled / agent_deleted / not_member / bad_spec。主人重新启用后清空。
    disabled_reason        text
);

CREATE INDEX idx_agent_schedules_owner ON agent_schedules(owner_id);
CREATE INDEX idx_agent_schedules_due
    ON agent_schedules (next_run_at)
    WHERE enabled AND next_run_at IS NOT NULL;

ALTER TABLE agent_invocations
    ALTER COLUMN trigger_message_id DROP NOT NULL;

ALTER TABLE agent_invocations
    ADD COLUMN schedule_id uuid REFERENCES agent_schedules(id) ON DELETE SET NULL;

CREATE INDEX idx_agent_invocations_schedule
    ON agent_invocations (schedule_id)
    WHERE schedule_id IS NOT NULL;

CREATE FUNCTION agent_invocations_origin() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.trigger_message_id IS NULL AND NEW.schedule_id IS NULL THEN
        IF TG_OP = 'UPDATE'
           AND OLD.schedule_id IS NOT NULL
           AND OLD.trigger_message_id IS NULL THEN
            RETURN NEW;
        END IF;
        RAISE EXCEPTION 'invocation needs trigger_message_id or schedule_id'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER agent_invocations_origin
    BEFORE INSERT OR UPDATE ON agent_invocations
    FOR EACH ROW
    EXECUTE PROCEDURE agent_invocations_origin();
