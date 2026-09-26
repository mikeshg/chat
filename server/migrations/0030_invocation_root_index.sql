-- 会话的调用历史同时按 root_conversation_id 取子调用（A15），给这一列加部分索引。
CREATE INDEX idx_agent_invocations_root
    ON agent_invocations(root_conversation_id, created_at DESC)
    WHERE root_conversation_id IS NOT NULL;
