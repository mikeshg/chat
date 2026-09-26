import { useEffect, useState } from "react";
import { agents, ApiError, convs } from "../api";
import { setState, useApp } from "../store";
import type { AgentHostView, AgentView, RegistryEntry } from "../types";
import { openConversation } from "../ws";
import { groupAgentsByHost, hostDisplayName } from "./agentHosts";
import AgentAvatar from "./AgentAvatar";
import AgentEditor from "./AgentEditor";
import AgentSettings from "./AgentSettings";
import RegistryPicker from "./RegistryPicker";
import PermissionTierField from "./PermissionTierField";

/** Agent A01：「我」页的 Agent 管理面板（宿主列表 / Agent 列表 / 添加 Agent） */
export default function AgentsPanel({ onClose }: { onClose: () => void }) {
  const s = useApp();
  const [list, setList] = useState<AgentView[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [name, setName] = useState("");
  const [hostId, setHostId] = useState("");
  const [command, setCommand] = useState("");
  const [args, setArgs] = useState("");
  const [cwd, setCwd] = useState("");
  const [runtimeTab, setRuntimeTab] = useState<"registry" | "custom">("registry");
  const [picked, setPicked] = useState<RegistryEntry | null>(null);
  const [permissionTier, setPermissionTier] = useState("read_only");
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editingHostId, setEditingHostId] = useState<string | null>(null);
  const [hostDraft, setHostDraft] = useState("");
  const [hostErr, setHostErr] = useState("");
  const [hostBusy, setHostBusy] = useState(false);

  const hosts = s.agentHosts; // online / label 由 agent_host_status 帧实时更新
  const groups = groupAgentsByHost(hosts, list);

  useEffect(() => {
    let alive = true;
    Promise.all([agents.hosts(), agents.list()])
      .then(([hs, as]) => {
        if (!alive) return;
        // 用 REST 真值初始化宿主表；online 之后由 WS 帧驱动
        setState({ agentHosts: hs });
        setList(as);
        setLoaded(true);
      })
      .catch(() => alive && setLoaded(true));
    return () => {
      alive = false;
    };
  }, []);

  // 可选项上报 / 删除后服务端推 invalidate agents
  useEffect(() => {
    const reload = () => {
      agents
        .list()
        .then(setList)
        .catch(() => {});
    };
    window.addEventListener("chat:agents", reload);
    return () => window.removeEventListener("chat:agents", reload);
  }, []);

  // 新宿主出现/表单打开时默认选中第一个
  useEffect(() => {
    if (!hostId && hosts.length > 0) setHostId(hosts[0].id);
  }, [hosts, hostId]);

  const hostLabel = (id: string) => {
    const h = hosts.find((x) => x.id === id);
    return h ? (h.label || "宿主") + (h.platform ? ` (${h.platform})` : "") : "未知宿主";
  };
  const hostOnline = (id: string) => hosts.find((x) => x.id === id)?.online ?? false;

  const saveHost = async (id: string) => {
    if (hostBusy) return;
    const label = hostDraft.trim();
    if (!label || [...label].length > 60) {
      setHostErr("名称需要 1–60 个字符");
      return;
    }
    setHostBusy(true);
    setHostErr("");
    try {
      const updated = await agents.patchHost(id, label);
      setState((cur) => ({
        agentHosts: cur.agentHosts.map((h) => (h.id === updated.id ? updated : h)),
      }));
      setEditingHostId(null);
    } catch (e) {
      setHostErr(
        e instanceof ApiError && e.message === "bad_label"
          ? "名称需要 1–60 个字符"
          : e instanceof Error
            ? e.message
            : "改名失败",
      );
    } finally {
      setHostBusy(false);
    }
  };

  const openAgentConv = async (a: AgentView) => {
    // 专属会话可能还没进本地列表（刚创建、invalidate 未刷新）：先刷目录再跳
    if (!s.conversations.some((c) => c.id === a.conversation_id)) {
      try {
        const list = await convs.list();
        setState({ conversations: list });
      } catch {}
    }
    onClose();
    openConversation(a.conversation_id);
  };

  const create = async () => {
    if (busy) return;
    setErr("");
    setBusy(true);
    try {
      const runtime =
        runtimeTab === "registry" && picked
          ? { registry: { id: picked.id, version: picked.version } }
          : {
              command: command.trim(),
              args: args
                .split("\n")
                .map((x) => x.trim())
                .filter(Boolean),
            };
      const a = await agents.create({
        name: name.trim(),
        host_id: hostId,
        runtime,
        cwd: cwd.trim(),
        permission_tier: permissionTier,
      });
      setList((l) => [...l, a]);
      setName("");
      setCommand("");
      setArgs("");
      setCwd("");
      setPicked(null);
      setPermissionTier("read_only");
      setShowForm(false);
    } catch (e) {
      // 400 bad_name / 409 name_taken / 404 宿主不存在 —— 保留服务端文案
      const msg =
        e instanceof ApiError && e.status === 409
          ? "名字已被占用（不区分大小写）"
          : e instanceof ApiError && e.status === 400
            ? `参数不合法：${e.message}`
            : e instanceof Error
              ? e.message
              : "创建失败";
      setErr(msg);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal agent-panel" onClick={(e) => e.stopPropagation()}>
        <div className="row" style={{ justifyContent: "space-between" }}>
          <h3>Agent</h3>
          <button className="icon-btn" onClick={onClose} aria-label="关闭">
            ✕
          </button>
        </div>

        <section>
          <h4 className="agent-sec">我的 Agent</h4>
          {groups.length === 0 && (
            <div className="muted">
              {loaded ? "还没有宿主。在桌面端登录后，这台电脑会自动成为宿主。" : "加载中…"}
            </div>
          )}
          {groups.map((g) => (
            <div
              key={g.host?.id ?? "unknown-host"}
              className="agent-host-group"
              role="group"
              aria-label={g.host ? hostDisplayName(g.host) : "未知宿主"}
            >
              {g.host ? (
                <HostTitle
                  host={g.host}
                  editing={editingHostId === g.host.id}
                  draft={hostDraft}
                  busy={hostBusy}
                  err={editingHostId === g.host.id ? hostErr : ""}
                  onDraft={setHostDraft}
                  onEdit={() => {
                    setEditingHostId(g.host!.id);
                    setHostDraft(g.host!.label);
                    setHostErr("");
                  }}
                  onCancel={() => {
                    setEditingHostId(null);
                    setHostErr("");
                  }}
                  onSave={() => void saveHost(g.host!.id)}
                />
              ) : (
                <div className="agent-host-head">
                  <span className="agent-host-meta">未知宿主</span>
                </div>
              )}
              {g.agents.length === 0 && (
                <div className="muted agent-host-empty">这个宿主上还没有 Agent</div>
              )}
              {g.agents.map((a) => (
                <div
                  key={a.id}
                  className="row agent-item clickable"
                  onClick={() => void openAgentConv(a)}
                  title="进入专属会话"
                >
                  <AgentAvatar agentId={a.id} name={a.name} version={a.avatar_version} size="sm" />
                  <span className="grow">
                    <b>{a.name}</b>
                  </span>
                  <i className={`online-dot ${hostOnline(a.host_id) ? "" : "off"}`} />
                  <button
                    type="button"
                    onClick={(e) => {
                      e.stopPropagation();
                      setShowForm(false);
                      setEditingId(a.id);
                    }}
                  >
                    编辑
                  </button>
                </div>
              ))}
            </div>
          ))}
          {editingId && list.some((a) => a.id === editingId) && (
            <AgentEditor
              agent={list.find((a) => a.id === editingId)!}
              hostLabel={hostLabel(list.find((a) => a.id === editingId)!.host_id)}
              onUpdated={(a) => setList((cur) => cur.map((x) => (x.id === a.id ? a : x)))}
              onSaved={(a) => {
                setList((cur) => cur.map((x) => (x.id === a.id ? a : x)));
                setEditingId(null);
              }}
              onDeleted={(id) => {
                setList((cur) => cur.filter((x) => x.id !== id));
                setEditingId(null);
              }}
              onClose={() => setEditingId(null)}
            />
          )}
        </section>

        <AgentSettings />

        {showForm ? (
          <section className="agent-form">
            <h4 className="agent-sec">添加 Agent</h4>
            <input
              placeholder="名字（1–32 位字母数字 _ -）"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
            <select
              aria-label="宿主"
              value={hostId}
              onChange={(e) => {
                setHostId(e.target.value);
                setPicked(null);
              }}
            >
              {hosts.map((h) => (
                <option key={h.id} value={h.id}>
                  {hostLabel(h.id)}
                  {h.online ? "" : "（离线）"}
                </option>
              ))}
            </select>
            <div className="agent-tabs" role="tablist" aria-label="运行时">
              <button
                type="button"
                role="tab"
                aria-selected={runtimeTab === "registry"}
                onClick={() => setRuntimeTab("registry")}
              >
                从登记表选择
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={runtimeTab === "custom"}
                onClick={() => setRuntimeTab("custom")}
              >
                自定义命令
              </button>
            </div>
            {runtimeTab === "registry" ? (
              <RegistryPicker hostId={hostId} selectedId={picked?.id ?? ""} onSelect={setPicked} />
            ) : (
              <>
                <input
                  aria-label="启动命令"
                  placeholder="启动命令（如 node / claude-code / codex）"
                  value={command}
                  onChange={(e) => setCommand(e.target.value)}
                />
                <textarea
                  aria-label="命令参数"
                  placeholder="参数（每行一个，可留空）"
                  value={args}
                  onChange={(e) => setArgs(e.target.value)}
                  rows={3}
                />
              </>
            )}
            <input
              aria-label="工作目录"
              placeholder="工作目录（宿主机上的绝对路径，可留空）"
              value={cwd}
              onChange={(e) => setCwd(e.target.value)}
            />
            <PermissionTierField value={permissionTier} onChange={setPermissionTier} />
            {err && <div className="auth-err">{err}</div>}
            <div className="row end">
              <button onClick={() => setShowForm(false)}>取消</button>
              <button
                className="primary"
                disabled={
                  busy ||
                  !name.trim() ||
                  !hostId ||
                  (runtimeTab === "custom" ? !command.trim() : picked?.status !== "ready")
                }
                onClick={create}
              >
                {busy ? "创建中…" : "创建"}
              </button>
            </div>
          </section>
        ) : (
          <button
            className="primary"
            disabled={hosts.length === 0}
            title={hosts.length === 0 ? "先在桌面端登录，让一台电脑成为宿主" : undefined}
            onClick={() => setShowForm(true)}
          >
            添加 Agent
          </button>
        )}
      </div>
    </div>
  );
}

function fmtSeen(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleString();
}

function HostTitle({
  host,
  editing,
  draft,
  busy,
  err,
  onDraft,
  onEdit,
  onCancel,
  onSave,
}: {
  host: AgentHostView;
  editing: boolean;
  draft: string;
  busy: boolean;
  err: string;
  onDraft: (v: string) => void;
  onEdit: () => void;
  onCancel: () => void;
  onSave: () => void;
}) {
  return (
    <>
      <div className="agent-host-head">
        <i className={`online-dot ${host.online ? "" : "off"}`} />
        {editing ? (
          <form
            className="agent-host-edit"
            onSubmit={(e) => {
              e.preventDefault();
              onSave();
            }}
          >
            <input
              aria-label="宿主名称"
              value={draft}
              maxLength={60}
              autoFocus
              onChange={(e) => onDraft(e.target.value)}
            />
            <button type="submit" className="primary" disabled={busy}>
              {busy ? "保存中…" : "保存"}
            </button>
            <button type="button" onClick={onCancel} disabled={busy}>
              取消
            </button>
          </form>
        ) : (
          <button type="button" className="agent-host-name" title="点击改名" onClick={onEdit}>
            {hostDisplayName(host)}
          </button>
        )}
        {host.platform ? <span className="agent-host-meta">{host.platform}</span> : null}
        <span className="agent-host-meta">{host.online ? "在线" : "离线"}</span>
        <span className="agent-host-meta">最后在线 {fmtSeen(host.last_seen_at)}</span>
      </div>
      {err && <div className="auth-err">{err}</div>}
    </>
  );
}
