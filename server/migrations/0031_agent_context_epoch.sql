-- Agent A21：上下文代次。开新话题把 epoch 加一并清空运行时会话；
-- 调用在下发前把当时的 epoch 记在自己的行上，完成时对不上就不写回。
-- context_epoch 为空表示迁移前已经派发的调用，完成时仍照常写回。

ALTER TABLE agent_contexts
    ADD COLUMN epoch bigint NOT NULL DEFAULT 0;

ALTER TABLE agent_invocations
    ADD COLUMN context_epoch bigint;
