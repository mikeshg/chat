import { useCallback, useEffect, useState } from "react";
import { agents } from "../../api";
import type { AgentOptionItem, AgentRuntime, ReportedOptions } from "../../types";

/** 内置 Agent 判定（A28）：runtime 形状就是 {"builtin": true} */
export function isBuiltin(runtime: AgentRuntime | undefined | null): boolean {
  return runtime?.builtin === true;
}

/**
 * 添加页选中一类程序后的可选项来源（A28）。
 * 内置走服务端 GET /agent-builtin/models（不调本机 probeLocal）；
 * 其余（登记表 / 自定义命令）在宿主电脑上试起程序读可选项。
 * deps 由调用方注入，测试用 mock 直接断言走了哪条路。
 */
export async function readPickOptions(
  kind: "builtin" | "program",
  deps: {
    probe: () => Promise<ReportedOptions>;
    builtinModels: () => Promise<AgentOptionItem[]>;
  },
): Promise<ReportedOptions> {
  if (kind === "builtin") return { models: await deps.builtinModels() };
  return deps.probe();
}

/** 内置 Agent 的模型可选项。active=false 时不请求；reload 重新拉一次。 */
export function useBuiltinModels(active: boolean): {
  items: AgentOptionItem[];
  loading: boolean;
  reload: () => void;
} {
  const [items, setItems] = useState<AgentOptionItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [seq, setSeq] = useState(0);
  useEffect(() => {
    if (!active) {
      setItems([]);
      return;
    }
    let alive = true;
    setLoading(true);
    agents
      .builtinModels()
      .then((l) => {
        if (alive) setItems(l);
      })
      .catch(() => {
        if (alive) setItems([]);
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [active, seq]);
  const reload = useCallback(() => setSeq((s) => s + 1), []);
  return { items, loading, reload };
}
