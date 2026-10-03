-- Bot 入站令牌改为多个：主人可以给不同程序各发一个，单独删除。
-- 入站改用 Authorization: Bearer <令牌>，令牌不再出现在地址里。
-- bots.token 改名 secret，只用来给回调请求签名；旧令牌搬进来，已有接入不用改签名校验。
CREATE TABLE bot_tokens (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    bot_id     uuid NOT NULL REFERENCES bots(user_id) ON DELETE CASCADE,
    name       text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 50),
    token      text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_bot_tokens_bot ON bot_tokens(bot_id);

INSERT INTO bot_tokens (bot_id, name, token) SELECT user_id, '默认', token FROM bots;
ALTER TABLE bots RENAME COLUMN token TO secret;
