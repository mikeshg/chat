// ===== V2 P1-3 =====
// 按会话草稿的本地持久化：localStorage 键 chat_drafts:<enc(server)>:<userId>，
// JSON 对象 {convId: entry}。读写都吞异常（隐私模式/损坏数据按空处理），
// 换服务器/换账号天然隔离。
//
// MM2：entry 从纯文本扩为 { text, reuse? }——reuse 是撤回重编辑挂上的
// 「复用附件」chip，随草稿一起持久化（服务端 TTL 内都能复用）。
// 旧格式（convId → 纯字符串）读入时自动升级；无 reuse 的条目仍按旧格式
// 写回纯字符串，降级到旧客户端不会丢草稿。

export interface DraftStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/** 「复用附件」chip（撤回重编辑带出）：再次发送时带 reuse_attachment_id 而不是重新上传 */
export interface DraftReuse {
  attachment_id: string;
  file_name: string;
  mime: string;
  size: number;
  kind: string; // "image" | "file" | ...（语音/投票不留 re-edit，不会出现 audio/poll）
  conversation_id: string;
  /** 本次复用发送在途的 client_msg_id：用于把 WS ack/error 关联回 chip */
  pendingClientMsgId?: string;
  /** 复用被服务端拒收（attachment_expired/forbidden）：chip 停留「附件已过期」，不再参与发送 */
  expired?: boolean;
}

export interface DraftEntry {
  text: string;
  reuse?: DraftReuse;
}

export function draftsKey(scope: string, userId: string): string {
  return `chat_drafts:${scope}:${userId}`;
}

function parseReuse(v: unknown): DraftReuse | undefined {
  const o = v as Partial<DraftReuse> | null;
  if (
    !o ||
    typeof o !== "object" ||
    typeof o.attachment_id !== "string" ||
    typeof o.file_name !== "string" ||
    typeof o.mime !== "string" ||
    typeof o.size !== "number" ||
    typeof o.kind !== "string" ||
    typeof o.conversation_id !== "string"
  ) {
    return undefined;
  }
  return {
    attachment_id: o.attachment_id,
    file_name: o.file_name,
    mime: o.mime,
    size: o.size,
    kind: o.kind,
    conversation_id: o.conversation_id,
    ...(typeof o.pendingClientMsgId === "string"
      ? { pendingClientMsgId: o.pendingClientMsgId }
      : {}),
    ...(o.expired === true ? { expired: true } : {}),
  };
}

function parseEntry(v: unknown): DraftEntry | null {
  if (typeof v === "string" && v) return { text: v }; // 旧格式：convId → 纯文本
  if (!v || typeof v !== "object" || Array.isArray(v)) return null;
  const o = v as Partial<DraftEntry>;
  const text = typeof o.text === "string" ? o.text : "";
  const reuse = parseReuse(o.reuse);
  if (!text && !reuse) return null;
  return { text, ...(reuse ? { reuse } : {}) };
}

/** 读取某用户在某服务器上的草稿表；缺失/损坏/类型不对一律返回 {} */
export function readDrafts(
  storage: DraftStorage,
  scope: string,
  userId: string,
): Record<string, DraftEntry> {
  try {
    const raw = storage.getItem(draftsKey(scope, userId));
    if (!raw) return {};
    const v: unknown = JSON.parse(raw);
    if (!v || typeof v !== "object" || Array.isArray(v)) return {};
    const out: Record<string, DraftEntry> = {};
    for (const [k, val] of Object.entries(v)) {
      const e = parseEntry(val);
      if (e) out[k] = e;
    }
    return out;
  } catch {
    return {};
  }
}

/** 覆写某用户在某服务器上的草稿表（失败静默，不阻塞输入）。无 reuse 的条目写回旧格式纯文本。 */
export function writeDrafts(
  storage: DraftStorage,
  scope: string,
  userId: string,
  drafts: Record<string, DraftEntry>,
): void {
  try {
    const obj = Object.fromEntries(
      Object.entries(drafts).map(([k, e]) => [k, e.reuse ? e : e.text]),
    );
    storage.setItem(draftsKey(scope, userId), JSON.stringify(obj));
  } catch {}
}

/**
 * 复用发送被服务端拒收（attachment_expired/forbidden/bad_request）：
 * chip 标为「附件已过期」（摘掉在途标记，不再参与发送），已发出的文字退回草稿。
 */
export function failReuseSend(
  drafts: Record<string, DraftEntry>,
  convId: string,
  attachmentId: string | undefined,
  content: string,
): Record<string, DraftEntry> {
  const cur = drafts[convId];
  const next: DraftEntry = {
    text: cur?.text ? (content ? `${cur.text}\n${content}` : cur.text) : content,
  };
  if (cur?.reuse && cur.reuse.attachment_id === attachmentId) {
    const { pendingClientMsgId: _p, ...rest } = cur.reuse;
    next.reuse = { ...rest, expired: true };
  } else if (cur?.reuse) {
    next.reuse = cur.reuse;
  }
  if (!next.text && !next.reuse) {
    const out = { ...drafts };
    delete out[convId];
    return out;
  }
  return { ...drafts, [convId]: next };
}

/** 复用发送被 ack：清掉对应 chip（文字草稿不动）；chip 已被用户摘掉时不动作 */
export function clearReuseOnAck(
  drafts: Record<string, DraftEntry>,
  convId: string,
  attachmentId: string | undefined,
): Record<string, DraftEntry> {
  const cur = drafts[convId];
  if (!cur?.reuse || cur.reuse.attachment_id !== attachmentId) return drafts;
  const next = { ...drafts };
  if (cur.text) next[convId] = { text: cur.text };
  else delete next[convId];
  return next;
}

/** 发送完成后输入框/草稿该留下什么：去掉已发出的前缀，只留发送期间新输入的部分；被改过的原样保留 */
export function unsentText(cur: string, sent: string): string {
  return sent && cur.startsWith(sent) ? cur.slice(sent.length).trimStart() : cur;
}
