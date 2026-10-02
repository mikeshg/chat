/** Agent A12：审批选项按钮文案、权限档位说明。纯函数，界面和测试共用。 */

export type PermissionTier = "read_only" | "workspace" | "full";

export interface TierChoice {
  id: PermissionTier;
  label: string;
  hint: string;
}

export const PERMISSION_TIERS: TierChoice[] = [
  {
    id: "read_only",
    label: "只读",
    hint: "读取、搜索、思考和抓取自动放行，其他操作需要你批准",
  },
  {
    id: "workspace",
    label: "可改工作目录",
    hint: "在只读基础上，工作目录和额外目录里的修改自动放行；执行命令和目录外的写入需要批准",
  },
  {
    id: "full",
    label: "完全放开",
    hint: "全部操作自动放行，不再询问",
  },
];

export function tierChoice(id: string | null | undefined): TierChoice {
  return PERMISSION_TIERS.find((t) => t.id === id) ?? PERMISSION_TIERS[0];
}

/** allow_once / allow_always / reject_once 用固定文案，其余用 agent 给的 name。 */
export function optionButtonLabel(kind: string | null | undefined, name: string | null | undefined): string {
  if (kind === "allow_once") return "本次允许";
  if (kind === "allow_always") return "总是允许";
  if (kind === "reject_once") return "拒绝";
  const n = (name ?? "").trim();
  return n || (kind ?? "").trim() || "选项";
}

const KIND_LABEL: Record<string, string> = {
  read: "读取",
  edit: "写入",
  delete: "删除",
  move: "移动",
  search: "搜索",
  execute: "执行",
  think: "思考",
  fetch: "抓取",
  switch_mode: "切换模式",
  other: "其他",
};

export function approvalKindLabel(kind: string | null | undefined): string {
  if (!kind) return "其他";
  return KIND_LABEL[kind] ?? kind;
}
