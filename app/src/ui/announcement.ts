/**
 * 公告更新时间晚于本设备已读时间时视为新公告。
 * 无已读记录算新；任一时间无法解析则不高亮（避免坏数据一直亮着）。
 */
export function isNewAnnouncement(updatedAt: string, seenAt: string | null): boolean {
  const updated = Date.parse(updatedAt);
  if (Number.isNaN(updated)) return false;
  if (!seenAt) return true;
  const seen = Date.parse(seenAt);
  if (Number.isNaN(seen)) return false;
  return updated > seen;
}

const seenKey = (convId: string) => `chat_ann_seen:${convId}`;

export function readAnnouncementSeen(convId: string): string | null {
  try {
    return localStorage.getItem(seenKey(convId));
  } catch {
    return null;
  }
}

export function writeAnnouncementSeen(convId: string, updatedAt: string) {
  try {
    localStorage.setItem(seenKey(convId), updatedAt);
  } catch {
    /* 隐私模式等环境写失败不影响展开 */
  }
}
