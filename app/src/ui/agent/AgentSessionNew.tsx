import { useState } from "react";
import { agents as agentsApi } from "../../api";
import type { AgentView, AgentWorkspace } from "../../types";
import { openConversation, refreshCatalogs, sendChat } from "../../ws";
import ChoiceCards from "./ChoiceCards";
import { OptionSelect } from "./ModelModeFields";
import { isBuiltin, useBuiltinModels } from "./agentBuiltin";
import { NO_AGENTS_ON_COMPUTER, agentErrText, folderName, optionName, programLine, sessionName } from "./agentPage";

/** A27：工作区里新开一条对话。选定 Agent 后这条对话里不能再换；模型、模式之后还能改。 */
export default function AgentSessionNew({
  workspace,
  agents,
  regNames,
  onBack,
}: {
  workspace: AgentWorkspace;
  /** 这台电脑上未删除的 Agent */
  agents: AgentView[];
  /** 登记表 id → 程序名 */
  regNames: Record<string, string>;
  onBack: () => void;
}) {
  const [agentId, setAgentId] = useState(agents[0]?.id ?? "");
  const [model, setModel] = useState("");
  const [mode, setMode] = useState("");
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const agent = agents.find((a) => a.id === agentId) ?? null;
  const opts = agent?.reported_options ?? {};
  // A28：内置 Agent 的模型选项来自服务端配置，没有模式
  const builtin = isBuiltin(agent?.runtime);
  const builtinModels = useBuiltinModels(builtin);
  const models = builtin ? builtinModels.items : opts.models;
  const follow = (id: string | undefined, items: { id: string; name: string }[] | undefined) =>
    `跟随 Agent（${id ? optionName(id, items) : "程序默认"}）`;

  const start = async () => {
    const body = text.trim();
    if (!agent || !body || busy) return;
    setBusy(true);
    setErr("");
    try {
      const { conversation_id } = await agentsApi.createSession(workspace.id, {
        agent_id: agent.id,
        name: sessionName(body),
        model,
        mode,
      });
      await refreshCatalogs();
      openConversation(conversation_id);
      sendChat(conversation_id, body);
    } catch (e) {
      setErr(agentErrText(e));
      setBusy(false);
    }
  };

  return (
    <section className="am-pane">
      <header className="am-head">
        <button type="button" className="am-back" onClick={onBack}>
          ← 返回
        </button>
        <div className="am-head-text">
          <h2>新对话 · {folderName(workspace.path)}</h2>
          <div className="am-sub">
            {workspace.path || "不在任何项目里：电脑会在 Chat 程序目录下建 tmp/<对话> 给它用"}
          </div>
        </div>
      </header>
      <div className="am-scroll">
        <div className="am-form">
          {agents.length === 0 ? (
            <div className="am-hint">{NO_AGENTS_ON_COMPUTER}。先在左边「Agent」里添加一个。</div>
          ) : (
            <>
              <div className="am-field">
                <span className="am-lb">Agent</span>
                <ChoiceCards
                  label="Agent"
                  value={agentId}
                  options={agents.map((a) => ({
                    id: a.id,
                    label: a.name,
                    hint: programLine(a.runtime, a.runtime?.registry ? regNames[a.runtime.registry.id] : undefined),
                  }))}
                  onChange={(id) => {
                    setAgentId(id);
                    setModel("");
                    setMode("");
                  }}
                />
                <span className="am-hint">开始后这条对话里不能换 Agent。</span>
              </div>
              <div className="am-field">
                <span className="am-lb">{builtin ? "模型" : "模型和模式"}</span>
                <div className="am-row">
                  <OptionSelect
                    label="模型"
                    value={model}
                    items={models ?? []}
                    defaultText={follow(agent?.model, models)}
                    onChange={setModel}
                  />
                  {!builtin && (
                    <OptionSelect
                      label="模式"
                      value={mode}
                      items={opts.modes ?? []}
                      defaultText={follow(agent?.mode, opts.modes)}
                      onChange={setMode}
                    />
                  )}
                </div>
                <span className="am-hint">对话里随时可以改，下一条消息起生效。</span>
              </div>
              <div className="am-field">
                <label htmlFor="ns-text">要做什么</label>
                <textarea
                  id="ns-text"
                  rows={5}
                  value={text}
                  autoFocus
                  placeholder="例如：看一下这个项目的测试为什么失败"
                  onChange={(e) => setText(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) void start();
                  }}
                />
              </div>
              {err && <div className="auth-err">{err}</div>}
              <div className="am-row end">
                <button type="button" onClick={onBack} disabled={busy}>
                  取消
                </button>
                <button
                  type="button"
                  className="primary"
                  disabled={busy || !agent || !text.trim()}
                  onClick={() => void start()}
                >
                  {busy ? "开始中…" : "开始"}
                </button>
              </div>
            </>
          )}
        </div>
      </div>
    </section>
  );
}
