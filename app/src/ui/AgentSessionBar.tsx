import { useEffect, useState } from "react";
import { agents as agentsApi } from "../api";
import { setState } from "../store";
import type { AgentView, ConversationView } from "../types";
import { OptionSelect } from "./ModelModeFields";
import { isBuiltin, useBuiltinModels } from "./agentBuiltin";
import { optionName } from "./agentPage";

/** A27：Agent 对话输入框上方的模型 / 模式。空值 = 跟随 Agent 配置；下一条消息起生效。 */
export default function AgentSessionBar({ conv }: { conv: ConversationView }) {
  const ref = conv.agent!;
  const [agent, setAgent] = useState<AgentView | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  useEffect(() => {
    let alive = true;
    const load = () =>
      agentsApi
        .list()
        .then((rows) => alive && setAgent(rows.find((a) => a.id === ref.id) ?? null))
        .catch(() => {});
    load();
    window.addEventListener("chat:agents", load);
    return () => {
      alive = false;
      window.removeEventListener("chat:agents", load);
    };
  }, [ref.id]);

  const options = agent?.reported_options ?? {};
  // A28：内置 Agent 的模型选项来自服务端配置，没有模式
  const builtin = isBuiltin(agent?.runtime);
  const builtinModels = useBuiltinModels(builtin);
  const models = builtin ? builtinModels.items : options.models;
  const save = async (body: { model?: string; mode?: string }) => {
    setBusy(true);
    setErr("");
    try {
      await agentsApi.patchSession(conv.id, body);
      setState((cur) => ({
        conversations: cur.conversations.map((c) =>
          c.id === conv.id && c.agent ? { ...c, agent: { ...c.agent, ...body } } : c,
        ),
      }));
    } catch {
      setErr("没改成，稍后再试");
    } finally {
      setBusy(false);
    }
  };
  // 选中后下拉框里也看得出是模型还是模式
  const labeled = (label: string, items: { id: string; name: string }[] | undefined) =>
    (items ?? []).map((o) => ({ ...o, name: `${label}：${o.name}` }));
  const follow = (id: string | undefined, items: { id: string; name: string }[] | undefined) =>
    `跟随 Agent（${id ? optionName(id, items) : "程序默认"}）`;

  return (
    <div className="agent-session-bar">
      <OptionSelect
        label="模型"
        value={ref.model ?? ""}
        items={labeled("模型", models)}
        defaultText={follow(agent?.model, models)}
        disabled={busy}
        onChange={(v) => void save({ model: v })}
      />
      {!builtin && (
        <OptionSelect
          label="模式"
          value={ref.mode ?? ""}
          items={labeled("模式", options.modes)}
          defaultText={follow(agent?.mode, options.modes)}
          disabled={busy}
          onChange={(v) => void save({ mode: v })}
        />
      )}
      {err && <span className="auth-err">{err}</span>}
    </div>
  );
}
