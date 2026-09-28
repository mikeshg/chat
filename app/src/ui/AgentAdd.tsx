import { useState } from "react";
import { agents } from "../api";
import type { AgentHostView, AgentView, RegistryEntry } from "../types";
import { hostDisplayName } from "./agentHosts";
import { agentErrText, platformLabel } from "./agentPage";
import { PERMISSION_TIERS } from "./approval";
import ChoiceCards from "./ChoiceCards";
import RegistryPicker from "./RegistryPicker";

type Pick =
  | { kind: "reg"; entry: RegistryEntry }
  | { kind: "cmd" }
  | null;

/** 只加到当前这台电脑。先选程序，选好后再填名字、工作目录和权限。 */
export default function AgentAdd({
  host,
  onCreated,
  onCancel,
}: {
  host: AgentHostView;
  onCreated: (agent: AgentView) => void;
  onCancel: () => void;
}) {
  const [query, setQuery] = useState("");
  const [pick, setPick] = useState<Pick>(null);
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [args, setArgs] = useState("");
  const [cwd, setCwd] = useState("");
  const [tier, setTier] = useState("read_only");
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);

  const ready = pick?.kind === "cmd" || (pick?.kind === "reg" && pick.entry.status === "ready");
  const os = platformLabel(host.platform);

  const selectEntry = (entry: RegistryEntry) => {
    setPick({ kind: "reg", entry });
    setName((cur) => (cur.trim() ? cur : entry.id));
    setErr("");
  };

  const create = async () => {
    if (busy || !pick || !ready) return;
    setBusy(true);
    setErr("");
    try {
      const runtime =
        pick.kind === "reg"
          ? { registry: { id: pick.entry.id, version: pick.entry.version } }
          : {
              command: command.trim(),
              args: args
                .split("\n")
                .map((x) => x.trim())
                .filter(Boolean),
            };
      const created = await agents.create({
        name: name.trim(),
        host_id: host.id,
        runtime,
        cwd: cwd.trim(),
        permission_tier: tier,
      });
      onCreated(created);
    } catch (e) {
      setErr(agentErrText(e));
    } finally {
      setBusy(false);
    }
  };

  const canCreate = !!name.trim() && ready && (pick?.kind !== "cmd" || !!command.trim());

  return (
    <section className="am-pane">
      <header className="am-head">
        <button type="button" className="am-back" onClick={onCancel}>
          ← 返回
        </button>
        <div className="am-head-text">
          <h2>添加 Agent</h2>
          <div className="am-sub">
            <i className={`am-dot${host.online ? " on" : ""}`} />
            运行在这台电脑：{hostDisplayName(host)}
            {os ? `（${os}）` : ""}
          </div>
        </div>
      </header>
      <div className="am-scroll">
        <div className="am-form am-form-wide">
          <div className="am-field">
            <span className="am-lb">1. 选择要运行的程序</span>
            <input
              id="n-search"
              type="search"
              placeholder="搜索程序，例如 claude"
              aria-label="搜索程序"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            <RegistryPicker hostId={host.id} query={query} selectedId={pick?.kind === "reg" ? pick.entry.id : ""} onSelect={selectEntry}>
              <button
                type="button"
                className={`am-pick${pick?.kind === "cmd" ? " on" : ""}`}
                onClick={() => {
                  setPick({ kind: "cmd" });
                  setErr("");
                }}
              >
                <b>自定义命令</b>
                <span className="am-st no">手动填写</span>
                <span className="am-ds">登记表里没有的 ACP 程序</span>
              </button>
            </RegistryPicker>
          </div>

          {ready && pick && (
            <>
              <div className="am-sec">2. 设置</div>
              {pick.kind === "cmd" && (
                <>
                  <div className="am-field">
                    <label htmlFor="n-cmd">启动命令</label>
                    <input
                      id="n-cmd"
                      placeholder="例如 node"
                      value={command}
                      onChange={(e) => setCommand(e.target.value)}
                    />
                  </div>
                  <div className="am-field">
                    <label htmlFor="n-args">参数</label>
                    <textarea
                      id="n-args"
                      rows={2}
                      placeholder="每行一个，例如 agent.js"
                      value={args}
                      onChange={(e) => setArgs(e.target.value)}
                    />
                  </div>
                </>
              )}
              <div className="am-field">
                <label htmlFor="n-name">名字</label>
                <input id="n-name" value={name} onChange={(e) => setName(e.target.value)} />
                <span className="am-hint">在会话里用 @{name.trim() || "名字"} 调用它。</span>
              </div>
              <div className="am-field">
                <label htmlFor="n-cwd">工作目录</label>
                <input
                  id="n-cwd"
                  value={cwd}
                  placeholder="例如 D:\work\project"
                  onChange={(e) => setCwd(e.target.value)}
                />
                <span className="am-hint">它读写文件的地方。可以留空，之后再改。</span>
              </div>
              <div className="am-field">
                <span className="am-lb">权限档位</span>
                <ChoiceCards
                  label="权限档位"
                  value={tier}
                  options={PERMISSION_TIERS.map((t) => ({
                    id: t.id,
                    label: t.id === "read_only" ? `${t.label}（推荐先用这个）` : t.label,
                    hint: t.hint,
                  }))}
                  onChange={setTier}
                />
              </div>
              {err && <div className="auth-err">{err}</div>}
              <div className="am-row end">
                <button type="button" onClick={onCancel}>
                  取消
                </button>
                <button type="button" className="primary" disabled={busy || !canCreate} onClick={() => void create()}>
                  {busy ? "创建中…" : "创建 Agent"}
                </button>
              </div>
            </>
          )}
          {!ready && err && <div className="auth-err">{err}</div>}
        </div>
      </div>
    </section>
  );
}
