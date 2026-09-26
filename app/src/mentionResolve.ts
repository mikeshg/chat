// 这条消息里的 @ 解析成谁。
// 显式选中优先；手打时先匹配自己的 Agent（不区分大小写），再匹配群成员。
// 调用方传入的群成员应已排除发送者本人。

export interface MentionAgent {
  id: string;
  name: string;
}

export interface MentionMember {
  id: string;
  username: string;
}

/** 补全里点中的一项。token 是插入的名字，不含 @，大小写与插入时一致。 */
export interface ExplicitTarget {
  kind: "agent" | "member";
  id: string;
  token: string;
}

export interface ResolveMentionsInput {
  text: string;
  explicit: ExplicitTarget[];
  myAgents: MentionAgent[];
  members: MentionMember[];
}

export interface ResolveMentionsResult {
  agentTargets: string[];
  memberMentions: string[];
}

interface AtToken {
  /** 含连字符的完整 token */
  name: string;
  /** 群成员用户名用的前缀（遇到 `-` 停止） */
  memberName: string;
}

const RE_AT = /@[A-Za-z0-9_][A-Za-z0-9_-]*/g;

function scanAt(text: string): AtToken[] {
  const out: AtToken[] = [];
  for (const m of text.matchAll(RE_AT)) {
    const name = m[0].slice(1);
    const cut = name.search(/[^A-Za-z0-9_]/);
    out.push({ name, memberName: cut < 0 ? name : name.slice(0, cut) });
  }
  return out;
}

/** 正文里已经删掉的显式选中项不再保留（一个 token 只对应一项）。 */
export function retainExplicit(text: string, explicit: ExplicitTarget[]): ExplicitTarget[] {
  const tokens = scanAt(text).map((t) => t.name);
  const used = new Set<number>();
  const kept: ExplicitTarget[] = [];
  for (const pick of explicit) {
    const i = tokens.findIndex((name, idx) => !used.has(idx) && name === pick.token);
    if (i >= 0) {
      used.add(i);
      kept.push(pick);
    }
  }
  return kept;
}

export function resolveMentions(input: ResolveMentionsInput): ResolveMentionsResult {
  const unused = input.explicit.slice();
  const agentTargets: string[] = [];
  const memberMentions: string[] = [];
  const seenAgent = new Set<string>();
  const seenMember = new Set<string>();

  for (const tok of scanAt(input.text)) {
    const idx = unused.findIndex((p) => p.token === tok.name);
    if (idx >= 0) {
      const pick = unused.splice(idx, 1)[0];
      if (pick.kind === "agent") {
        if (!seenAgent.has(pick.id)) {
          seenAgent.add(pick.id);
          agentTargets.push(pick.id);
        }
      } else if (!seenMember.has(pick.id)) {
        seenMember.add(pick.id);
        memberMentions.push(pick.id);
      }
      continue;
    }
    const agent = input.myAgents.find((a) => a.name.toLowerCase() === tok.name.toLowerCase());
    if (agent) {
      if (!seenAgent.has(agent.id)) {
        seenAgent.add(agent.id);
        agentTargets.push(agent.id);
      }
      continue;
    }
    const member = input.members.find((m) => m.username === tok.memberName);
    if (member && !seenMember.has(member.id)) {
      seenMember.add(member.id);
      memberMentions.push(member.id);
    }
  }
  return { agentTargets, memberMentions };
}
