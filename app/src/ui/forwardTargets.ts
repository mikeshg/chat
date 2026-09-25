import type { ConversationView, FriendView } from "../types";

/**
 * 转发/收藏可选目标列表（V2 P2-4）：
 * - 文件传输助手（self）置顶，其余保持会话列表既有顺序；
 * - 排除 peer 已不再是好友的只读 DM；好友列表未加载（空）时不过滤，
 *   交由服务端逐条返回错误（result.ok=false / error）。
 */
export function forwardTargets(
  convs: ConversationView[],
  friends: FriendView[],
): ConversationView[] {
  const friendIds = new Set(friends.map((f) => f.user.id));
  const writable = convs.filter(
    (c) =>
      c.kind !== "dm" ||
      c.peer == null ||
      friends.length === 0 ||
      friendIds.has(c.peer.id),
  );
  return [...writable].sort(
    (a, b) => Number(b.kind === "self") - Number(a.kind === "self"),
  );
}
