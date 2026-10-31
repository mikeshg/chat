import { describe, expect, it } from "vitest";
import { groupFriendsByLetter, sectionLetter } from "./contactsAz";
import type { FriendView, UserPublic } from "../types";

function u(partial: Partial<UserPublic> & Pick<UserPublic, "id" | "username" | "display_name">): UserPublic {
  return { role: "user", last_seen_at: null, avatar_version: 0, has_avatar: false, is_bot: false, ...partial };
}
function f(name: string, id = name): FriendView {
  return { user: u({ id, username: id, display_name: name }), since: "2020-01-01T00:00:00Z" };
}

describe("sectionLetter", () => {
  it("A–Z", () => {
    expect(sectionLetter("alice")).toBe("A");
    expect(sectionLetter("Bob")).toBe("B");
  });
  it("# for CJK/empty", () => {
    expect(sectionLetter("张三")).toBe("#");
    expect(sectionLetter("")).toBe("#");
  });
});

describe("groupFriendsByLetter", () => {
  it("groups; # last", () => {
    const sections = groupFriendsByLetter([f("bob"), f("alice"), f("张三"), f("Adam")]);
    expect(sections.map((s) => s.letter)).toEqual(["A", "B", "#"]);
    expect(sections[0]!.friends.map((x) => x.user.display_name)).toEqual(["Adam", "alice"]);
  });
});
