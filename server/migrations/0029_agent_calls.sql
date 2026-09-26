-- Agent A14：Agent 互调。
-- 子调用挂在父调用上；深度和根会话给嵌套展示与帧扇出。
-- context_key / prompt 不在任务单的列清单里，但离线补发和成功收尾都要原样用到：
-- 触发消息只是「被谁调用」的记录（截断 200 字），不是完整指令，也不是 call:<上下文号>。

ALTER TABLE agent_invocations
    ADD COLUMN parent_invocation_id uuid REFERENCES agent_invocations(id) ON DELETE SET NULL,
    ADD COLUMN depth integer NOT NULL DEFAULT 0,
    ADD COLUMN root_conversation_id uuid,
    ADD COLUMN result_text text,
    ADD COLUMN context_key text,
    ADD COLUMN prompt text;

CREATE INDEX idx_agent_invocations_parent
    ON agent_invocations (parent_invocation_id)
    WHERE parent_invocation_id IS NOT NULL;

ALTER TABLE agent_user_settings
    ADD COLUMN max_call_depth integer NOT NULL DEFAULT 3
        CHECK (max_call_depth BETWEEN 1 AND 10);

-- 来源：触发消息 / 定时任务 / 父调用 至少一个。
-- 外键 ON DELETE SET NULL 会抹掉唯一来源，这两步放行（与删时间表同理）。
CREATE OR REPLACE FUNCTION agent_invocations_origin() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.trigger_message_id IS NULL
       AND NEW.schedule_id IS NULL
       AND NEW.parent_invocation_id IS NULL THEN
        IF TG_OP = 'UPDATE' THEN
            IF OLD.schedule_id IS NOT NULL
               AND OLD.trigger_message_id IS NULL
               AND OLD.parent_invocation_id IS NULL THEN
                RETURN NEW;
            END IF;
            IF OLD.parent_invocation_id IS NOT NULL
               AND OLD.trigger_message_id IS NULL
               AND OLD.schedule_id IS NULL THEN
                RETURN NEW;
            END IF;
        END IF;
        RAISE EXCEPTION 'invocation needs trigger_message_id or schedule_id or parent_invocation_id'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
