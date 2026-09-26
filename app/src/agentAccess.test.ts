import { describe, expect, it } from "vitest";
import { canUseAgent, mentionCandidates, type MentionAgent, type MentionUser } from "./agentAccess";

const user = (username: string): MentionUser => ({
  kind: "user",
  id: `u-${username}`,
  username,
  displayName: username,
});

const agent = (username: string): MentionAgent => ({
  kind: "agent",
  id: `a-${username}`,
  username,
  displayName: username,
  online: true,
});

describe("canUseAgent", () => {
  it("false 才关闭，缺省仍可用", () => {
    expect(canUseAgent(null)).toBe(false);
    expect(canUseAgent({ agent_access: false })).toBe(false);
    expect(canUseAgent({ agent_access: true })).toBe(true);
    expect(canUseAgent({})).toBe(true);
  });
});

describe("mentionCandidates", () => {
  const users = [user("ada")];
  const agents = [agent("bot")];

  it("无权时不列 Agent", () => {
    expect(mentionCandidates(users, agents, false)).toEqual(users);
  });

  it("有权时 Agent 在成员前面", () => {
    expect(mentionCandidates(users, agents, true).map((x) => x.kind)).toEqual(["agent", "user"]);
  });
});
