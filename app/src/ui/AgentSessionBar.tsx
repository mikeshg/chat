import { useEffect, useState } from "react";
import { agents as agentsApi } from "../api";
import { setState } from "../store";
import type { AgentOptionItem, AgentView, ConversationView } from "../types";
import { isBuiltin, useBuiltinModels } from "./agentBuiltin";
import { groupOptions } from "./agentPage";

/** A27：Agent 对话输入框工具栏里的模型 / 模式。空值 = 跟随 Agent 配置；下一条消息起生效。 */
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
  return (
    <div className="agent-session-bar">
      <GroupedSelect
        label="模型"
        value={ref.model}
        own={agent?.model}
        items={models}
        disabled={busy}
        onChange={(v) => void save({ model: v })}
      />
      {!builtin && (
        <GroupedSelect
          label="模式"
          value={ref.mode}
          own={agent?.mode}
          items={options.modes}
          disabled={busy}
          onChange={(v) => void save({ mode: v })}
        />
      )}
      {err && <span className="auth-err">{err}</span>}
    </div>
  );
}

/** 两级菜单：第一层供应商，悬停（触屏点按）在右侧展开该供应商的模型；按钮只显示模型名。
 *  选中 Agent 自己的那个 = 存空值（跟随 Agent） */
function GroupedSelect({
  label,
  value,
  own,
  items,
  disabled,
  onChange,
}: {
  label: string;
  value: string | undefined;
  own: string | undefined;
  items: AgentOptionItem[] | undefined;
  disabled: boolean;
  onChange: (v: string) => void;
}) {
  const [pos, setPos] = useState<{ left: number; bottom: number } | null>(null);
  const [openGroup, setOpenGroup] = useState("");
  const shown = value || own || "";
  const { loose, groups } = groupOptions(items);
  const name = shown
    ? [...loose, ...groups.flatMap(([, its]) => its)].find((o) => o.id === shown)?.name ?? shown
    : "默认";
  const close = () => {
    setPos(null);
    setOpenGroup("");
  };
  useEffect(() => {
    if (!pos) return;
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && close();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [pos]);
  const item = (o: AgentOptionItem, top = false) => (
    <button
      key={o.id}
      type="button"
      className={`ctx-item mp-item ${o.id === shown ? "on" : ""}`}
      onMouseEnter={top ? () => setOpenGroup("") : undefined}
      onClick={() => {
        close();
        if (o.id !== shown) onChange(o.id === own ? "" : o.id);
      }}
    >
      {o.name}
    </button>
  );
  return (
    <>
      <button
        type="button"
        className="mp-trigger"
        aria-label={label}
        aria-haspopup="menu"
        disabled={disabled}
        onClick={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          setPos({ left: r.left, bottom: window.innerHeight - r.top + 4 });
        }}
      >
        {name} <span aria-hidden>▾</span>
      </button>
      {pos && (
        <div className="ctx-mask" onClick={close}>
          <div className="ctx-menu mp-menu" role="menu" style={pos} onClick={(e) => e.stopPropagation()}>
            {!own && item({ id: "", name: "默认" }, true)}
            {loose.map((o) => item(o, true))}
            {groups.map(([g, its]) => (
              <div
                key={g}
                className={`mp-group ${openGroup === g ? "on" : ""}`}
                onMouseEnter={() => setOpenGroup(g)}
              >
                <button
                  type="button"
                  className={`ctx-item mp-item ${its.some((o) => o.id === shown) ? "on" : ""}`}
                  onClick={() => setOpenGroup(g)}
                >
                  {g} <span aria-hidden>›</span>
                </button>
                {openGroup === g && <div className="ctx-menu mp-sub" role="menu">{its.map((o) => item(o))}</div>}
              </div>
            ))}
          </div>
        </div>
      )}
    </>
  );
}
