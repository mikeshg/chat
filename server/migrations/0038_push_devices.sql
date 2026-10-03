-- 推送改为每台手机一个主题：可以单独移除丢失的手机，不必让其余手机重新订阅。
-- 开关与 preview 仍按用户存在 push_settings；旧的单一主题搬成一台「默认」设备，已订阅的手机不用改。
CREATE TABLE push_devices (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 50),
    -- 服务端生成的随机主题：chat-<24 位小写字母数字>
    topic      text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_push_devices_user ON push_devices(user_id);

INSERT INTO push_devices (user_id, name, topic) SELECT user_id, '默认', topic FROM push_settings;
ALTER TABLE push_settings DROP COLUMN topic;
