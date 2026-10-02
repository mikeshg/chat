import { describe, expect, it } from "vitest";
import { convTitle } from "../../store";
import { forwardTargets } from "./forwardTargets";
import type { ConversationView, FriendView, UserPublic } from "../../types";

function user(id: string): UserPublic {
  return { id, username: id, display_name: id, role: "user", last_seen_at: null };
}

function conv(id: string, patch: Partial<ConversationView> = {}): ConversationView {
  return {
    id,
    kind: "dm",
    name: null,
    owner_id: null,
    peer: null,
    member_count: 2,
    last_seq: 0,
    unread_count: 0,
    has_mention: false,
    last_message: null,
    preference: null,
    created_at: "2026-01-01T00:00:00Z",
    ...patch,
  };
}

const friend = (id: string): FriendView => ({ user: user(id), since: "2026-01-01T00:00:00Z" });

describe("V2 P2-4 转发目标列表", () => {
  it("文件传输助手置顶，其余保持会话列表顺序", () => {
    const list = [
      conv("dm1", { peer: user("u1") }),
      conv("self", { kind: "self", member_count: 1 }),
      conv("g1", { kind: "group", name: "g1" }),
      conv("dm2", { peer: user("u2") }),
    ];
    expect(
      forwardTargets(list, [friend("u1"), friend("u2")]).map((c) => c.id),
    ).toEqual(["self", "dm1", "g1", "dm2"]);
  });

  it("排除 peer 已不是好友的只读 DM；群与 self 不受影响", () => {
    const list = [
      conv("dm-dead", { peer: user("gone") }),
      conv("self", { kind: "self", member_count: 1 }),
      conv("dm-live", { peer: user("u1") }),
      conv("g1", { kind: "group", name: "g1" }),
    ];
    expect(forwardTargets(list, [friend("u1")]).map((c) => c.id)).toEqual([
      "self",
      "dm-live",
      "g1",
    ]);
  });

  it("好友列表未加载时不过滤 DM（交由服务端逐条报错）", () => {
    const list = [conv("dm1", { peer: user("u1") })];
    expect(forwardTargets(list, []).map((c) => c.id)).toEqual(["dm1"]);
  });
});

describe("V2 P2-4 convTitle", () => {
  it("kind=self → 文件传输助手", () => {
    expect(convTitle(conv("self", { kind: "self", member_count: 1 }))).toBe(
      "文件传输助手",
    );
  });
});
