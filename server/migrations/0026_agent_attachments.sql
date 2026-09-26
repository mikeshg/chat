-- A05：调用附件。
-- agent_invocations.attachment_ids：这次调用带走的主人附件（派发时收集、入库即定，
-- 读模型/宿主帧/前端提示共用一份事实）。
-- attachments.agent_invocation_id：产出文件是经哪次调用的凭据上传的，
-- invocation_finish.files 据此做归属校验。
ALTER TABLE agent_invocations
    ADD COLUMN attachment_ids uuid[] NOT NULL DEFAULT '{}';

ALTER TABLE attachments
    ADD COLUMN agent_invocation_id uuid REFERENCES agent_invocations(id) ON DELETE SET NULL;

-- finish.files 的归属校验按调用反查
CREATE INDEX idx_attachments_agent_invocation
    ON attachments(agent_invocation_id)
    WHERE agent_invocation_id IS NOT NULL;
