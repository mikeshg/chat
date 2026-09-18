-- 账号级角色与禁用标记。
-- role 为账号角色（admin/user），与会话内 memberships.role 无关。
-- 存量最早注册的用户自动成为管理员；之后由管理员在后台任命/撤销。

ALTER TABLE users ADD COLUMN role text NOT NULL DEFAULT 'user' CHECK (role IN ('admin','user'));
ALTER TABLE users ADD COLUMN disabled_at timestamptz;

UPDATE users SET role = 'admin'
WHERE id = (SELECT id FROM users ORDER BY created_at, id LIMIT 1);
