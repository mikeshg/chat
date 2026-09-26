// ===== V2 P1-3 =====
// 按会话草稿的本地持久化：localStorage 键 chat_drafts:<userId>，JSON 对象 {convId: text}。
// 读写都吞异常（隐私模式/损坏数据按空处理），换账号天然隔离。

export interface DraftStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export function draftsKey(userId: string): string {
  return `chat_drafts:${userId}`;
}

/** 读取某用户的草稿表；缺失/损坏/类型不对一律返回 {} */
export function readDrafts(storage: DraftStorage, userId: string): Record<string, string> {
  try {
    const raw = storage.getItem(draftsKey(userId));
    if (!raw) return {};
    const v: unknown = JSON.parse(raw);
    if (!v || typeof v !== "object" || Array.isArray(v)) return {};
    const out: Record<string, string> = {};
    for (const [k, val] of Object.entries(v)) {
      if (typeof val === "string" && val) out[k] = val;
    }
    return out;
  } catch {
    return {};
  }
}

/** 覆写某用户的草稿表（失败静默，不阻塞输入） */
export function writeDrafts(
  storage: DraftStorage,
  userId: string,
  drafts: Record<string, string>,
): void {
  try {
    storage.setItem(draftsKey(userId), JSON.stringify(drafts));
  } catch {}
}

/** 发送完成后输入框/草稿该留下什么：去掉已发出的前缀，只留发送期间新输入的部分；被改过的原样保留 */
export function unsentText(cur: string, sent: string): string {
  return sent && cur.startsWith(sent) ? cur.slice(sent.length).trimStart() : cur;
}
