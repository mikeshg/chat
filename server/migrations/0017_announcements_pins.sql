-- 群公告与会话置顶消息（V2 P3-2）。
-- 公告挂在 conversations 上（仅群聊使用）；置顶独立成表，删除消息时级联清理。

ALTER TABLE conversations
    ADD COLUMN announcement    text CHECK (announcement IS NULL OR char_length(announcement) BETWEEN 1 AND 2000),
    ADD COLUMN announcement_by uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN announcement_at timestamptz;

CREATE TABLE conversation_pins (
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    message_id      uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    pinned_by       uuid REFERENCES users(id) ON DELETE SET NULL,
    pinned_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation_id, message_id)
);
