import { describe, expect, it } from "vitest";
import type { AgentHostView, AgentView } from "../../types";
import { groupAgentsByHost, hostDisplayName } from "./agentHosts";

function host(partial: Partial<AgentHostView> & { id: string }): AgentHostView {
  return {
    label: partial.id,
    platform: "windows",
    online: false,
    last_seen_at: "2026-01-01T00:00:00Z",
    ...partial,
  };
}

function agent(id: string, hostId: string): AgentView {
  return {
    id,
    name: id,
    host_id: hostId,
    host_online: false,
    runtime: { command: "node", args: [] },
    cwd: "",
    conversation_id: `c-${id}`,
    created_at: "2026-01-01T00:00:00Z",
  };
}

describe("Agent A09：按宿主分组", () => {
  it("展示名去掉首尾空白，空名显示为「电脑」", () => {
    expect(hostDisplayName(host({ id: "h", label: "  家里  " }))).toBe("家里");
    expect(hostDisplayName(host({ id: "h", label: "" }))).toBe("电脑");
    expect(hostDisplayName(host({ id: "h", label: "   " }))).toBe("电脑");
  });

  it("在线在前，其次按名称，同名按 id", () => {
    const hosts = [
      host({ id: "z-off", label: "a-off", online: false }),
      host({ id: "b", label: "beta", online: true }),
      host({ id: "a2", label: "alpha", online: true }),
      host({ id: "a1", label: "alpha", online: true }),
    ];
    const ids = hosts.map((h) => h.id);
    const groups = groupAgentsByHost(hosts, []);
    expect(groups.map((g) => g.host?.id)).toEqual(["a1", "a2", "b", "z-off"]);
    // 不改传入的数组
    expect(hosts.map((h) => h.id)).toEqual(ids);
  });

  it("Agent 归到对应宿主下，组内保持原顺序；没有 Agent 的宿主仍占一组", () => {
    const hosts = [
      host({ id: "h-on", label: "on", online: true }),
      host({ id: "h-off", label: "off", online: false }),
    ];
    const agents = [agent("second", "h-on"), agent("first", "h-on"), agent("night", "h-off")];
    const groups = groupAgentsByHost(hosts, agents);
    expect(groups.map((g) => g.host?.id)).toEqual(["h-on", "h-off"]);
    expect(groups[0].agents.map((a) => a.id)).toEqual(["second", "first"]);
    expect(groups[1].agents.map((a) => a.id)).toEqual(["night"]);
  });

  it("对不上宿主的 Agent 单独成组，排在最后", () => {
    const groups = groupAgentsByHost(
      [host({ id: "h", label: "pc", online: true })],
      [agent("known", "h"), agent("lost", "missing")],
    );
    expect(groups).toHaveLength(2);
    expect(groups[1].host).toBeNull();
    expect(groups[1].agents.map((a) => a.id)).toEqual(["lost"]);
  });

  it("都为空时没有分组", () => {
    expect(groupAgentsByHost([], [])).toEqual([]);
  });
});
