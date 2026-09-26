-- V2 P3-1 全局搜索：trigram 索引支撑 messages.content 的 ILIKE 子串匹配
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- 只索引未删除消息的正文：删除时 content 已被占位文本覆写且 deleted_at 置位，
-- 部分索引让墓碑行自动退出索引（也不会按「消息已删除」命中）。
CREATE INDEX idx_messages_content_trgm
    ON messages USING gin (content gin_trgm_ops)
    WHERE deleted_at IS NULL;
