-- 会话偏好：按用户的置顶/静音/归档（§3.5/§8.3）。偏好是用户维度，不改变会话本身。
-- mute_forever 与 muted_until 互斥：muted_until IS NULL 仅表示无定时静音（不代表永久）。

CREATE TABLE conversation_preferences (
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    user_id         uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    pinned_at       timestamptz,
    archived_at     timestamptz,
    -- 永久静音标记；与 muted_until 互斥
    mute_forever    boolean NOT NULL DEFAULT false,
    -- 定时静音截止时间；NULL = 无定时静音
    muted_until     timestamptz,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation_id, user_id),
    CONSTRAINT conv_pref_mute_shape CHECK (NOT mute_forever OR muted_until IS NULL)
);
CREATE INDEX idx_conv_prefs_user ON conversation_preferences(user_id);
