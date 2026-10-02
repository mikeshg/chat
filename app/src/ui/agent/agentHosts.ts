import type { AgentHostView, AgentView } from "../../types";

/** 组标题用的电脑名：去掉首尾空白，空则「电脑」。排序也用这个名字。 */
export function hostDisplayName(host: AgentHostView): string {
  const name = host.label.trim();
  return name || "电脑";
}

export interface AgentHostGroup {
  /** 对不上任何宿主时为 null，这种组排在最后 */
  host: AgentHostView | null;
  agents: AgentView[];
}

/**
 * Agent 按宿主分组。
 * 排序：在线在前，其次按名称（zh），同名按 id。组内 Agent 保持传入顺序。
 * 没有 Agent 的宿主仍然占一组。不修改传入的数组。
 */
export function groupAgentsByHost(hosts: AgentHostView[], agents: AgentView[]): AgentHostGroup[] {
  const sorted = [...hosts].sort((a, b) => {
    if (a.online !== b.online) return a.online ? -1 : 1;
    const byName = hostDisplayName(a).localeCompare(hostDisplayName(b), "zh");
    if (byName !== 0) return byName;
    return a.id.localeCompare(b.id);
  });
  const known = new Set(sorted.map((h) => h.id));
  const groups: AgentHostGroup[] = sorted.map((host) => ({
    host,
    agents: agents.filter((a) => a.host_id === host.id),
  }));
  const orphans = agents.filter((a) => !known.has(a.host_id));
  if (orphans.length > 0) groups.push({ host: null, agents: orphans });
  return groups;
}
