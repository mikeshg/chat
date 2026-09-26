import type { AgentRuntime } from "../types";

/** 登记表条目在添加表单里怎么显示：能不能选、要不要安装按钮、原因文案。 */
export interface RegistryEntryUi {
  selectable: boolean;
  /** 非空时显示这个按钮 */
  installLabel: string | null;
  /** 非空时显示原因，条目不可选 */
  reasonLabel: string | null;
  statusText: string;
}

export function registryEntryUi(status: string, reason?: string): RegistryEntryUi {
  if (status === "ready") {
    return { selectable: true, installLabel: null, reasonLabel: null, statusText: "可用" };
  }
  if (status === "installable") {
    return { selectable: false, installLabel: "安装", reasonLabel: null, statusText: "可安装" };
  }
  return {
    selectable: false,
    installLabel: null,
    reasonLabel: reason?.trim() || "当前环境不支持",
    statusText: "不可用",
  };
}

/** 编辑页运行时一行：登记表用名称和版本，否则是自定义命令。 */
export function runtimeLabel(runtime: AgentRuntime | undefined, displayName?: string): string {
  const pin = runtime?.registry;
  if (pin?.id && pin.version) {
    const name = displayName?.trim() || pin.id;
    return `登记表：${name} ${pin.version}`;
  }
  const cmd = [runtime?.command?.trim() ?? "", ...(runtime?.args ?? [])].filter(Boolean).join(" ");
  return cmd ? `自定义命令：${cmd}` : "自定义命令";
}
