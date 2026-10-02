-- Webhook bot：一个属于创建者（owner）的特殊账号。
-- 账号本身是 users 的一行（会话、成员、消息、@ 都照常用），不能登录、不能被搜到。
-- 主人和它自动成为好友、建私聊；主人可以把它拉进自己所在的群。
ALTER TABLE users ADD COLUMN bot_owner_id uuid REFERENCES users(id) ON DELETE CASCADE;

CREATE TABLE bots (
    user_id     uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    owner_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- 出站：bot 被私聊或在群里被 @ 时 POST 到这里；空串 = 不回调
    webhook_url text NOT NULL DEFAULT '' CHECK (char_length(webhook_url) <= 500),
    -- 入站：POST /v1/hooks/{token} 以 bot 身份发消息；出站请求用它做 HMAC 签名
    token       text NOT NULL UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_bots_owner ON bots(owner_id);
