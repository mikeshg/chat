-- 全局可配项与注册邀请码（V2 P1-2）。
-- server_settings：key → jsonb 的松散配置存储；缺省键按代码默认值处理（不预置行），
-- 后续阶段（存储配额、会议地址等）继续往里加键。

CREATE TABLE server_settings (
    key        text PRIMARY KEY,
    value      jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by uuid REFERENCES users(id) ON DELETE SET NULL
);

CREATE TABLE invites (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code       text NOT NULL UNIQUE,
    note       text NOT NULL DEFAULT '' CHECK (char_length(note) <= 100),
    created_by uuid REFERENCES users(id) ON DELETE SET NULL,
    max_uses   integer CHECK (max_uses IS NULL OR max_uses > 0),  -- NULL = 不限次数
    uses       integer NOT NULL DEFAULT 0,
    expires_at timestamptz,                                       -- NULL = 永不过期
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- 审计：通过哪个邀请码注册（邀请码删除不影响用户）
ALTER TABLE users ADD COLUMN invite_id uuid REFERENCES invites(id) ON DELETE SET NULL;
