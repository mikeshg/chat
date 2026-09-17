CREATE TABLE attachments (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    uploader_id  uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    conv_id      uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    file_name    text NOT NULL,
    mime         text NOT NULL,
    size         bigint NOT NULL,
    storage_path text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_attachments_conv ON attachments(conv_id);

ALTER TABLE messages
    ADD COLUMN kind           text NOT NULL DEFAULT 'text',
    ADD COLUMN attachment_id  uuid REFERENCES attachments(id) ON DELETE SET NULL;
