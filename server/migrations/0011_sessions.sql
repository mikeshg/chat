-- 登录会话（= 一台设备的一次登录）。refresh 轮换沿用同一 session，
-- 撤销 session 即踢掉该设备的全部凭据与连接。

CREATE TABLE sessions (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_label text NOT NULL DEFAULT '' CHECK (char_length(device_label) <= 60),
    user_agent   text NOT NULL DEFAULT '' CHECK (char_length(user_agent) <= 300),
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz NOT NULL DEFAULT now(),
    revoked_at   timestamptz
);
CREATE INDEX idx_sessions_user ON sessions(user_id) WHERE revoked_at IS NULL;

ALTER TABLE refresh_tokens ADD COLUMN session_id uuid;

-- 为现存的有效 refresh token（未撤销、未过期）各补建一条「旧设备」session 并回填；
-- 已撤销/过期的行 session_id 留 NULL（refresh 到它们时统一 401）。
UPDATE refresh_tokens SET session_id = gen_random_uuid()
 WHERE revoked_at IS NULL AND expires_at > now();
INSERT INTO sessions (id, user_id, device_label, created_at, last_used_at)
SELECT session_id, user_id, '旧设备', created_at, created_at
  FROM refresh_tokens WHERE session_id IS NOT NULL;

ALTER TABLE refresh_tokens
    ADD CONSTRAINT fk_refresh_tokens_session
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE;
CREATE INDEX idx_refresh_tokens_session ON refresh_tokens(session_id);
