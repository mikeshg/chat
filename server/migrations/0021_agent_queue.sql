-- A04：排队截止时间。NULL = 永不超时（生效超时为 0）。
ALTER TABLE agent_invocations
    ADD COLUMN queue_deadline timestamptz;

-- 清扫只扫还在排队、且设了截止时间的行
CREATE INDEX idx_agent_invocations_queue_deadline
    ON agent_invocations (queue_deadline)
    WHERE status = 'queued' AND queue_deadline IS NOT NULL;
