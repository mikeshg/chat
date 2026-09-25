-- V2 P2-4：文件传输助手 —— 为 0015 之前已存在的用户回填 type='self' 单人会话。
-- 幂等：uq_self_conversation（0012）按 owner_id 去重，memberships PK 兜底重复执行。

INSERT INTO conversations (type, owner_id)
SELECT 'self', u.id FROM users u
ON CONFLICT (owner_id) WHERE type = 'self' DO NOTHING;

-- 每个 self 会话恰好一行 owner 成员（含异常缺失时的补写）
INSERT INTO memberships (conversation_id, user_id, role)
SELECT c.id, c.owner_id, 'owner'
  FROM conversations c
 WHERE c.type = 'self' AND c.owner_id IS NOT NULL
ON CONFLICT DO NOTHING;
