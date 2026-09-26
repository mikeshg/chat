import { useEffect, useState } from "react";
import { agents, ApiError, convs } from "../api";
import { setState, useApp } from "../store";
import type { AgentView } from "../types";
import { openConversation } from "../ws";
import AgentAvatar from "./AgentAvatar";
import AgentEditor from "./AgentEditor";
import AgentSettings from "./AgentSettings";

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
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);

  const hosts = s.agentHosts; // online 由 agent_host_status 帧实时更新

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
      const a = await agents.create({
        name: name.trim(),
        host_id: hostId,
        runtime: {
          command: command.trim(),
          args: args
            .split("\n")
            .map((x) => x.trim())
            .filter(Boolean),
        },
        cwd: cwd.trim(),
      });
      setList((l) => [...l, a]);
      setName("");
      setCommand("");
      setArgs("");
      setCwd("");
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
          <h4 className="agent-sec">我的宿主（这台电脑/其他电脑上的桌面端）</h4>
          {hosts.length === 0 && (
            <div className="muted">
              {loaded ? "还没有宿主。在桌面端登录后，这台电脑会自动成为宿主。" : "加载中…"}
            </div>
          )}
          {hosts.map((h) => (
            <div key={h.id} className="row agent-item">
              <i className={`online-dot ${h.online ? "" : "off"}`} />
              <span className="grow">
                {h.label || "宿主"}
                {h.platform ? <span className="muted"> · {h.platform}</span> : null}
              </span>
              <span className="muted">{h.online ? "在线" : "离线"}</span>
            </div>
          ))}
        </section>

        <section>
          <h4 className="agent-sec">我的 Agent</h4>
          {list.length === 0 && (
            <div className="muted">{loaded ? "还没有 Agent。" : "加载中…"}</div>
          )}
          {list.map((a) => (
            <div
              key={a.id}
              className="row agent-item clickable"
              onClick={() => void openAgentConv(a)}
              title="进入专属会话"
            >
              <AgentAvatar agentId={a.id} name={a.name} version={a.avatar_version} size="sm" />
              <span className="grow">
                <b>{a.name}</b>
                <span className="muted"> · {hostLabel(a.host_id)}</span>
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
            <select value={hostId} onChange={(e) => setHostId(e.target.value)}>
              {hosts.map((h) => (
                <option key={h.id} value={h.id}>
                  {hostLabel(h.id)}
                  {h.online ? "" : "（离线）"}
                </option>
              ))}
            </select>
            <input
              placeholder="启动命令（如 node / claude-code / codex）"
              value={command}
              onChange={(e) => setCommand(e.target.value)}
            />
            <textarea
              placeholder="参数（每行一个，可留空）"
              value={args}
              onChange={(e) => setArgs(e.target.value)}
              rows={3}
            />
            <input
              placeholder="工作目录（宿主机上的绝对路径，可留空）"
              value={cwd}
              onChange={(e) => setCwd(e.target.value)}
            />
            {err && <div className="auth-err">{err}</div>}
            <div className="row end">
              <button onClick={() => setShowForm(false)}>取消</button>
              <button
                className="primary"
                disabled={busy || !name.trim() || !hostId || !command.trim()}
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
