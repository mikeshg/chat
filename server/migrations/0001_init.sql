CREATE EXTENSION IF NOT EXISTS citext;

CREATE TABLE users (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    username      citext NOT NULL UNIQUE,
    display_name  text NOT NULL,
    password_hash text NOT NULL,
    last_seen_at  timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT username_format CHECK (username ~ '^[a-zA-Z0-9_]{3,20}$')
);

CREATE TABLE refresh_tokens (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash   text NOT NULL UNIQUE,
    device_label text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    revoked_at   timestamptz
);
CREATE INDEX idx_refresh_tokens_user ON refresh_tokens(user_id);

CREATE TABLE friend_requests (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    from_user   uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    to_user     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    message     text NOT NULL DEFAULT '',
    status      text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','accepted','declined')),
    created_at  timestamptz NOT NULL DEFAULT now(),
    resolved_at timestamptz,
    CONSTRAINT no_self_request CHECK (from_user <> to_user)
);
-- 同一方向最多一条 pending
CREATE UNIQUE INDEX uq_friend_request_pending
    ON friend_requests(from_user, to_user) WHERE status = 'pending';
CREATE INDEX idx_friend_requests_to ON friend_requests(to_user) WHERE status = 'pending';

CREATE TABLE friendships (
    user_a     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    user_b     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_a, user_b),
    CONSTRAINT canonical_order CHECK (user_a < user_b)
);

CREATE TABLE conversations (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    type       text NOT NULL CHECK (type IN ('dm','group')),
    name       text,
    owner_id   uuid REFERENCES users(id) ON DELETE SET NULL,
    last_seq   bigint NOT NULL DEFAULT 0,
    dm_key     text,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX uq_dm_key ON conversations(dm_key) WHERE type = 'dm';
CREATE INDEX idx_conversations_owner ON conversations(owner_id);

CREATE TABLE memberships (
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    user_id         uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role            text NOT NULL DEFAULT 'member' CHECK (role IN ('owner','member')),
    last_read_seq   bigint NOT NULL DEFAULT 0,
    joined_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation_id, user_id)
);
CREATE INDEX idx_memberships_user ON memberships(user_id);

CREATE TABLE messages (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    seq             bigint NOT NULL,
    sender_id       uuid NOT NULL REFERENCES users(id),
    content         text NOT NULL CHECK (char_length(content) BETWEEN 1 AND 4000),
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (conversation_id, seq)
);
CREATE INDEX idx_messages_conv_seq ON messages(conversation_id, seq DESC);
