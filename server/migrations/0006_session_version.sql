-- 会话版本：禁用/重置密码时 +1，使旧 access token 与 WS ticket 立即失效。
ALTER TABLE users ADD COLUMN session_version integer NOT NULL DEFAULT 0;
