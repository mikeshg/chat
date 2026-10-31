/** Same sender within 2 minutes → Bubble Group (GLOSSARY). */
export const BUBBLE_GROUP_MS = 2 * 60 * 1000;

export function inBubbleGroup(
  cur: { sender_id: string; created_at: string },
  prev: { sender_id: string; created_at: string } | null | undefined,
): boolean {
  if (!prev) return false;
  if (prev.sender_id !== cur.sender_id) return false;
  const dt = new Date(cur.created_at).getTime() - new Date(prev.created_at).getTime();
  return dt >= 0 && dt <= BUBBLE_GROUP_MS;
}
