/** 生效值。缺省（旧服务端）视为可用，显式 false 才关掉。 */
export function canUseAgent(me: { agent_access?: boolean } | null | undefined): boolean {
  return !!me && me.agent_access !== false;
}

export interface MentionUser {
  kind: "user";
  id: string;
  username: string;
  displayName: string;
}

export interface MentionAgent {
  kind: "agent";
  id: string;
  username: string;
  displayName: string;
  online: boolean;
}

export type MentionChoice = MentionUser | MentionAgent;

/**
 * @ 补全。无权时不列 Agent，群成员保留。
 * 有权时 Agent 排在成员前面（A11 把主人的 Agent 填进 agents）。
 */
export function mentionCandidates(
  users: MentionUser[],
  agents: MentionAgent[],
  allowAgents: boolean,
): MentionChoice[] {
  if (!allowAgents) return users;
  return [...agents, ...users];
}
