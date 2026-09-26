-- ntfy 离线推送（V2 P3-3）。
-- 推送服务器地址只由管理员经 server_settings 配置（防 SSRF）；本表存每用户的开关与
-- 服务端生成的随机订阅主题。公共 ntfy 服务器上主题可被猜到的人看到，
-- 故 preview（通知带消息内容）默认关闭。

CREATE TABLE push_settings (
    user_id    uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    enabled    boolean NOT NULL DEFAULT false,
    -- 服务端生成的随机主题：chat-<24 位小写字母数字>
    topic      text NOT NULL UNIQUE,
    -- 通知里是否带消息内容；默认不带
    preview    boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now()
);
