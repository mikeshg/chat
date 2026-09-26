import { describe, expect, it } from "vitest";
import { resolveMentions, retainExplicit, type ExplicitTarget } from "./mentionResolve";

const bot = { id: "agent-bot", name: "bot" };
const bot1 = { id: "agent-1", name: "bot1" };
const bot2 = { id: "agent-2", name: "bot2" };
const memberBot = { id: "user-bot", username: "bot" };
const alice = { id: "user-alice", username: "alice" };

const agentPick = (id: string, token: string): ExplicitTarget => ({ kind: "agent", id, token });
const memberPick = (id: string, token: string): ExplicitTarget => ({ kind: "member", id, token });

describe("resolveMentions", () => {
  it("显式选中 Agent 时同名成员不再被提及", () => {
    const r = resolveMentions({
      text: "@bot echo:a",
      explicit: [agentPick(bot.id, "bot")],
      myAgents: [bot],
      members: [memberBot, alice],
    });
    expect(r.agentTargets).toEqual([bot.id]);
    expect(r.memberMentions).toEqual([]);
  });

  it("显式选中同名成员时不调用 Agent", () => {
    const r = resolveMentions({
      text: "@bot 你好",
      explicit: [memberPick(memberBot.id, "bot")],
      myAgents: [bot],
      members: [memberBot],
    });
    expect(r.agentTargets).toEqual([]);
    expect(r.memberMentions).toEqual([memberBot.id]);
  });

  it("手打未选时优先解析为自己的 Agent，不提及同名成员", () => {
    const r = resolveMentions({
      text: "@bot echo:a",
      explicit: [],
      myAgents: [bot],
      members: [memberBot, alice],
    });
    expect(r.agentTargets).toEqual([bot.id]);
    expect(r.memberMentions).toEqual([]);
  });

  it("手打 Agent 名字不区分大小写", () => {
    const r = resolveMentions({
      text: "@BoT echo:a",
      explicit: [],
      myAgents: [bot],
      members: [memberBot],
    });
    expect(r.agentTargets).toEqual([bot.id]);
    expect(r.memberMentions).toEqual([]);
  });

  it("没有同名 Agent 时手打 @ 提及群成员", () => {
    const r = resolveMentions({
      text: "hi @alice",
      explicit: [],
      myAgents: [bot],
      members: [memberBot, alice],
    });
    expect(r.agentTargets).toEqual([]);
    expect(r.memberMentions).toEqual([alice.id]);
  });

  it("一条消息里的两个 Agent 都进入目标", () => {
    const r = resolveMentions({
      text: "@bot1 @bot2 echo:z",
      explicit: [],
      myAgents: [bot1, bot2],
      members: [],
    });
    expect(r.agentTargets).toEqual([bot1.id, bot2.id]);
    expect(r.memberMentions).toEqual([]);
  });

  it("删掉对应 @名字 后显式选中不再算数", () => {
    const explicit = [agentPick(bot.id, "bot")];
    expect(retainExplicit("echo:a", explicit)).toEqual([]);
    const r = resolveMentions({
      text: "echo:a",
      explicit: retainExplicit("echo:a", explicit),
      myAgents: [bot],
      members: [memberBot],
    });
    expect(r.agentTargets).toEqual([]);
    expect(r.memberMentions).toEqual([]);
  });
});
