import { ApiError } from "../api";
import type { AgentHostView, AgentOptionItem, AgentOutputMode, AgentRuntime, AgentView, ConversationView } from "../types";
import { tierChoice } from "./approval";

/** 输出方式。hint 是单选卡片上的说明。 */
export const OUTPUT_MODE_OPTIONS: { v: AgentOutputMode; label: string; hint: string }[] = [
  { v: "stream", label: "实时流式", hint: "边生成边显示文字和正在做的步骤" },
  { v: "status", label: "状态 + 最终结果", hint: "只显示当前在做哪一步，完成后发一条回复" },
  { v: "steps", label: "每步一条消息", hint: "每完成一步就在会话里发一条消息" },
];

/** 排队超时选项（秒），0 = 永不 */
export const QUEUE_TIMEOUT_OPTIONS: { v: number; label: string }[] = [
  { v: 600, label: "10 分钟" },
  { v: 3600, label: "1 小时" },
  { v: 6 * 3600, label: "6 小时" },
  { v: 86400, label: "1 天" },
  { v: 7 * 86400, label: "7 天" },
  { v: 0, label: "永不" },
];

export const NO_AGENTS_ON_COMPUTER = "这台电脑上还没有 Agent";
export const ADD_ON_DESKTOP_HINT = "要添加 Agent，在想让它运行的那台电脑上打开 Chat 桌面端。";

/** 界面上的系统名。代码里的 platform 标识不改。 */
export function platformLabel(platform: string | null | undefined): string {
  const p = (platform ?? "").trim();
  if (p === "windows") return "Windows";
  if (p === "macos") return "macOS";
  if (p === "linux") return "Linux";
  return p;
}

/**
 * 电脑组标题右侧的状态。
 * 在线只写系统与「在线」；离线才附上「最后在线 …」。
 */
export function hostGroupStatus(platform: string, online: boolean, lastSeenLabel: string): string {
  const os = platformLabel(platform);
  const head = os ? `${os} · ` : "";
  if (online) return `${head}在线`;
  const seen = lastSeenLabel.trim();
  return seen ? `${head}离线，最后在线 ${seen}` : `${head}离线`;
}

/** 桌面端才知道本机；网页 / Android / 宿主未就绪时 localHostId 为空。测试可注入 host id。 */
export function resolveLocalComputer(opts: {
  desktopShell: boolean;
  storeHostId: string | null;
  injectedHostId: string | null;
}): { desktop: boolean; localHostId: string | null } {
  const injected = opts.injectedHostId?.trim() || null;
  if (injected) return { desktop: true, localHostId: injected };
  return {
    desktop: opts.desktopShell,
    localHostId: opts.desktopShell ? opts.storeHostId : null,
  };
}

/** 读测试页注入的本机 host id。正式桌面端不设置这个值。 */
export function readInjectedLocalHostId(): string | null {
  const id = (globalThis as { __CHAT_LOCAL_HOST__?: unknown }).__CHAT_LOCAL_HOST__;
  return typeof id === "string" && id.trim() ? id.trim() : null;
}

export function showLocalTag(
  local: { desktop: boolean; localHostId: string | null },
  hostId: string,
): boolean {
  return local.desktop && !!local.localHostId && local.localHostId === hostId;
}

/** 「＋ 添加」：桌面端，且本机这台电脑当前在线。 */
export function canAddAgent(
  local: { desktop: boolean; localHostId: string | null },
  hosts: { id: string; online: boolean }[],
): boolean {
  if (!local.desktop || !local.localHostId) return false;
  return hosts.some((h) => h.id === local.localHostId && h.online);
}

/** 登记表搜索：名字或 id，忽略大小写。自定义命令卡片不在这张表里，调用方始终另画。 */
export function filterPrograms<T extends { id: string; name: string }>(entries: T[], query: string): T[] {
  const q = query.trim().toLowerCase();
  if (!q) return entries;
  return entries.filter((e) => e.name.toLowerCase().includes(q) || e.id.toLowerCase().includes(q));
}

export function outputModeLabel(mode: string | null | undefined): string {
  return OUTPUT_MODE_OPTIONS.find((o) => o.v === mode)?.label ?? OUTPUT_MODE_OPTIONS[0].label;
}

export function queueTimeoutLabel(secs: number | null | undefined): string {
  if (secs == null) return QUEUE_TIMEOUT_OPTIONS.find((o) => o.v === 86400)?.label ?? "";
  return QUEUE_TIMEOUT_OPTIONS.find((o) => o.v === secs)?.label ?? `${secs} 秒`;
}

/** 输出方式卡片第一项的副文案。 */
export function followDefaultOutputHint(currentLabel: string): string {
  return `现在是「${currentLabel}」`;
}

/** 离线排队下拉的第一项。 */
export function followDefaultQueueOption(currentLabel: string): string {
  return `跟随默认设置（现在是 ${currentLabel}）`;
}

/** 列表一行右侧：程序名 · 权限档位。 */
export function programLine(runtime: AgentRuntime | undefined, registryName?: string): string {
  const pin = runtime?.registry;
  if (pin?.id) return registryName?.trim() || pin.id;
  const cmd = runtime?.command?.trim();
  return cmd ? `自定义命令 · ${cmd}` : "自定义命令";
}

export function agentRowMeta(
  runtime: AgentRuntime | undefined,
  tier: string | null | undefined,
  registryName?: string,
): string {
  return `${programLine(runtime, registryName)} · ${tierChoice(tier).label}`;
}

/** 详情头部：运行在 X（本机）· 在线 / 离线，新调用会排队 */
export function agentPlaceLine(name: string, local: boolean, online: boolean): string {
  const where = local ? `${name}（本机）` : name;
  return online ? `运行在 ${where} · 在线` : `运行在 ${where} · 离线，新调用会排队`;
}

/** 会话副标题里的电脑名。空名显示「电脑」。 */
export function computerTitle(label: string | null | undefined, platform: string | null | undefined): string {
  const name = (label ?? "").trim() || "电脑";
  const os = (platform ?? "").trim();
  return os ? `${name} (${os})` : name;
}

/** 前四个标签共用的草稿。定时任务不在这里，改了就立即保存。 */
export interface AgentDraftFields {
  name: string;
  command: string;
  argsText: string;
  cwd: string;
  extraDirs: string[];
  model: string;
  mode: string;
  instructions: string;
  envRows: { key: string; value: string }[];
  outputMode: string;
  queueTimeout: string;
  permissionTier: string;
}

export function draftFromAgent(agent: AgentView): AgentDraftFields {
  const env = agent.env ?? {};
  return {
    name: agent.name,
    command: agent.runtime?.command ?? "",
    argsText: (agent.runtime?.args ?? []).join("\n"),
    cwd: agent.cwd ?? "",
    extraDirs: [...(agent.extra_dirs ?? [])],
    model: agent.model ?? "",
    mode: agent.mode ?? "",
    instructions: agent.instructions ?? "",
    envRows: Object.entries(env).map(([key, value]) => ({ key, value })),
    outputMode: agent.output_mode ?? "",
    queueTimeout: agent.queue_timeout_secs == null ? "" : String(agent.queue_timeout_secs),
    permissionTier: agent.permission_tier || "read_only",
  };
}

function draftKey(d: AgentDraftFields): string {
  return JSON.stringify({
    ...d,
    extraDirs: [...d.extraDirs],
    envRows: d.envRows.map((r) => ({ key: r.key, value: r.value })),
    permissionTier: d.permissionTier || "read_only",
  });
}

/** 草稿和已保存的配置不一致。 */
export function agentDraftDirty(saved: AgentDraftFields, draft: AgentDraftFields): boolean {
  return draftKey(saved) !== draftKey(draft);
}

/** 底部保存条。没改动时不出现。 */
export function agentSaveBar(dirty: boolean): { visible: boolean; text: string } {
  return dirty ? { visible: true, text: "有未保存的修改" } : { visible: false, text: "" };
}

/** 创建 / 保存失败时给用户看的句子。沿用原来编辑表单的错误码。 */
export function agentErrText(e: unknown): string {
  if (!(e instanceof ApiError)) return e instanceof Error ? e.message : "保存失败";
  const known: Record<string, string> = {
    name_taken: "名字已被占用（不区分大小写）",
    bad_name: "名字不合法（1–32 位字母数字 _ -，不能以 - 开头）",
    bad_runtime: "命令、参数或工作目录不合法",
    bad_extra_dirs: "额外目录过多或某一行过长（最多 10 行，每行 500 字）",
    bad_instructions: "附加指令超过 8000 字",
    bad_env: "环境变量不合法（最多 50 项，键 100 字、值 4000 字）",
    bad_model: "模型名过长",
    bad_mode: "模式名过长",
    host_immutable: "不能更换电脑",
    bad_output_mode: "输出方式不合法",
    bad_queue_timeout: "排队超时不合法",
    bad_permission_tier: "权限档位不合法",
    host_offline: "电脑不在线",
    host_not_local: "只能添加到正在运行 Chat 桌面端的这台电脑",
    bad_label: "名称需要 1–60 个字符",
    probe_timeout: "读取超时，程序可能卡在启动或登录",
    bad_path: "路径不合法（1–500 字，不能有控制字符）",
    agent_on_other_host: "这个 Agent 不在这台电脑上",
    invocation_running: "还有没结束的调用，先取消或等它结束",
  };
  if (known[e.message]) return known[e.message];
  if (e.status === 409) return known.name_taken;
  if (e.status === 400) return `参数不合法：${e.message}`;
  if (e.status === 404) return "找不到这台电脑或这个 Agent";
  return e.message || "保存失败";
}

// ===== Agent A27：工作区与对话 =====

/** 可选项 id → 名字；列表里没有就显示 id */
export function optionName(id: string, items: AgentOptionItem[] | undefined): string {
  return items?.find((o) => o.id === id)?.name ?? id;
}

/** 路径最后一段当工作区名（兼容 / 和 \） */
export function folderName(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

/** Agent 页选哪台电脑：保持当前 → 本机 → 第一台在线 → 第一台 */
export function pickHost(hosts: AgentHostView[], localId: string | null, current: string | null): string | null {
  if (current && hosts.some((h) => h.id === current)) return current;
  if (localId && hosts.some((h) => h.id === localId)) return localId;
  return (hosts.find((h) => h.online) ?? hosts[0])?.id ?? null;
}

/** 工作区下的对话，最近活动在前 */
export function workspaceSessions(convs: ConversationView[], wsId: string): ConversationView[] {
  const at = (c: ConversationView) => c.last_message?.created_at ?? c.created_at;
  return convs
    .filter((c) => c.agent?.workspace_id === wsId)
    .sort((a, b) => (at(a) < at(b) ? 1 : at(a) > at(b) ? -1 : 0));
}

/** 新对话的名字：第一条消息的第一行，最多 30 字 */
export function sessionName(text: string): string {
  const line = text.trim().split("\n")[0].trim();
  return [...line].slice(0, 30).join("");
}
