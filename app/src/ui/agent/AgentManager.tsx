import { useEffect, useState } from "react";
import { isDesktopShell } from "../../agentHost";
import { agents, convs } from "../../api";
import { convTitle, isWorkspaceConv, setState, useApp } from "../../store";
import type { AgentUserSettings, AgentView, AgentWorkspace, ConversationView } from "../../types";
import { openConversation } from "../../ws";
import { hostDisplayName } from "./agentHosts";
import {
  ADD_ON_DESKTOP_HINT,
  NO_AGENTS_ON_COMPUTER,
  agentErrText,
  agentRowMeta,
  canAddAgent,
  folderName,
  hostGroupStatus,
  pickHost,
  readInjectedLocalHostId,
  resolveLocalComputer,
  showLocalTag,
  workspaceSessions,
} from "./agentPage";
import AgentAdd from "./AgentAdd";
import AgentAvatar from "./AgentAvatar";
import AgentDetail from "./AgentDetail";
import AgentSessionNew from "./AgentSessionNew";
import AgentSettings from "./AgentSettings";
import ChatView from "../chat/ChatView";
import DirPicker from "./DirPicker";

type Pane =
  | { kind: "blank" }
  | { kind: "agent"; id: string }
  | { kind: "add" }
  | { kind: "defaults" }
  | { kind: "new"; wsId: string }
  | { kind: "session"; id: string };

function fmtSeen(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const m = d.getMonth() + 1;
  const day = d.getDate();
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return `${m}/${day} ${hh}:${mm}`;
}

/** Agent 页：先选电脑，再看它的工作区和对话；下方是这台电脑上的 Agent 配置。 */
export default function AgentManager() {
  const s = useApp();
  const [list, setList] = useState<AgentView[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [view, setView] = useState<Pane>({ kind: "blank" });
  const [pending, setPending] = useState<Pane | null>(null);
  const [dirty, setDirty] = useState(false);
  const [menu, setMenu] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [nameDraft, setNameDraft] = useState("");
  const [rowErr, setRowErr] = useState("");
  const [rowBusy, setRowBusy] = useState(false);
  const [confirmDel, setConfirmDel] = useState<string | null>(null);
  const [settings, setSettings] = useState<AgentUserSettings | null>(null);
  const [names, setNames] = useState<Record<string, string>>({});
  const [banner, setBanner] = useState<{ id: string; text: string } | null>(null);
  const [hostSel, setHostSel] = useState<string | null>(null);
  const [workspaces, setWorkspaces] = useState<AgentWorkspace[]>([]);
  const [addingWs, setAddingWs] = useState(false);
  const [wsPath, setWsPath] = useState("");
  const [browsing, setBrowsing] = useState(false);

  const local = resolveLocalComputer({
    desktopShell: isDesktopShell(),
    storeHostId: s.localHostId,
    injectedHostId: readInjectedLocalHostId(),
  });
  const hosts = s.agentHosts;
  const hostId = pickHost(hosts, local.localHostId, hostSel);
  const host = hosts.find((h) => h.id === hostId) ?? null;
  const hostAgents = list.filter((a) => a.host_id === hostId);
  const allowAdd = canAddAgent(local, hosts);
  const localHost = hosts.find((h) => h.id === local.localHostId) ?? null;
  const hostKey = hosts.map((h) => `${h.id}:${h.online}`).join("|");
  const regNames: Record<string, string> = {};
  for (const [k, v] of Object.entries(names)) {
    if (k.startsWith(`${hostId}:`)) regNames[k.slice(`${hostId}:`.length)] = v;
  }

  useEffect(() => {
    let alive = true;
    Promise.all([agents.hosts(), agents.list(), agents.settings()])
      .then(([hs, as, settings]) => {
        if (!alive) return;
        setState({ agentHosts: hs });
        setList(as);
        setSettings(settings);
        setLoaded(true);
      })
      .catch(() => alive && setLoaded(true));
    return () => {
      alive = false;
    };
  }, []);

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

  useEffect(() => {
    if (!hostId) {
      setWorkspaces([]);
      return;
    }
    let alive = true;
    const load = () =>
      agents
        .workspaces(hostId)
        .then((ws) => alive && setWorkspaces(ws))
        .catch(() => alive && setWorkspaces([]));
    load();
    window.addEventListener("chat:workspaces", load);
    return () => {
      alive = false;
      window.removeEventListener("chat:workspaces", load);
    };
  }, [hostId]);

  useEffect(() => {
    let alive = true;
    for (const h of hosts) {
      if (!h.online) continue;
      agents
        .registry(h.id)
        .then((res) => {
          if (!alive) return;
          setNames((prev) => {
            const next = { ...prev };
            for (const e of res.entries ?? []) next[`${h.id}:${e.id}`] = e.name;
            return next;
          });
        })
        .catch(() => {});
    }
    return () => {
      alive = false;
    };
  }, [hostKey]);

  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    window.addEventListener("click", close);
    return () => window.removeEventListener("click", close);
  }, [menu]);

  // 通知、提示点进来的工作区对话：切到它的电脑并打开
  const activeConv = s.conversations.find((c) => c.id === s.activeConv);
  useEffect(() => {
    if (!activeConv || !isWorkspaceConv(activeConv)) return;
    if (view.kind === "session" && view.id === activeConv.id) return;
    setHostSel(activeConv.agent!.host_id);
    setView({ kind: "session", id: activeConv.id });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeConv?.id]);

  const go = (next: Pane) => {
    setPending(null);
    setMenu(null);
    setView(next);
    if (next.kind === "session") openConversation(next.id);
    else if (isWorkspaceConv(activeConv)) setState({ activeConv: null });
  };

  const requestNav = (next: Pane) => {
    const leaving = view.kind === "agent" && !(next.kind === "agent" && next.id === view.id);
    if (dirty && leaving) {
      setPending(next);
      return;
    }
    go(next);
  };

  const startRename = (id: string, current: string) => {
    setRenaming(id);
    setNameDraft(current);
    setRowErr("");
    setMenu(null);
  };

  const saveHost = async (id: string) => {
    if (rowBusy) return;
    const label = nameDraft.trim();
    if (!label || [...label].length > 60) {
      setRowErr("名称需要 1–60 个字符");
      return;
    }
    setRowBusy(true);
    setRowErr("");
    try {
      const updated = await agents.patchHost(id, label);
      setState((cur) => ({
        agentHosts: cur.agentHosts.map((h) => (h.id === updated.id ? updated : h)),
      }));
      setRenaming(null);
    } catch (e) {
      setRowErr(agentErrText(e));
    } finally {
      setRowBusy(false);
    }
  };

  const saveSessionName = async (id: string) => {
    if (rowBusy) return;
    setRowBusy(true);
    setRowErr("");
    try {
      await agents.patchSession(id, { name: nameDraft.trim() });
      const name = nameDraft.trim() || null;
      setState((cur) => ({
        conversations: cur.conversations.map((c) => (c.id === id ? { ...c, name } : c)),
      }));
      setRenaming(null);
    } catch (e) {
      setRowErr(agentErrText(e));
    } finally {
      setRowBusy(false);
    }
  };

  const deleteSession = async (id: string) => {
    setRowBusy(true);
    setRowErr("");
    try {
      await agents.removeSession(id);
      setConfirmDel(null);
      if (view.kind === "session" && view.id === id) go({ kind: "blank" });
      setState((cur) => ({ conversations: cur.conversations.filter((c) => c.id !== id) }));
    } catch (e) {
      setRowErr(agentErrText(e));
    } finally {
      setRowBusy(false);
    }
  };

  const addWorkspace = async (path = wsPath) => {
    if (!hostId || rowBusy) return;
    setRowBusy(true);
    setRowErr("");
    try {
      const ws = await agents.addWorkspace(hostId, path);
      setWorkspaces((cur) => (cur.some((w) => w.id === ws.id) ? cur : [...cur, ws]));
      setAddingWs(false);
      setBrowsing(false);
      setWsPath("");
    } catch (e) {
      setRowErr(agentErrText(e));
    } finally {
      setRowBusy(false);
    }
  };

  const removeWorkspace = async (ws: AgentWorkspace) => {
    setMenu(null);
    try {
      await agents.removeWorkspace(ws.id);
      setWorkspaces((cur) => cur.filter((w) => w.id !== ws.id));
      const open = view.kind === "session" ? s.conversations.find((c) => c.id === view.id) : null;
      if ((view.kind === "new" && view.wsId === ws.id) || open?.agent?.workspace_id === ws.id) {
        go({ kind: "blank" });
      }
      setBanner({ id: "", text: `已移除「${folderName(ws.path)}」，再加同一路径可以找回它的对话` });
    } catch (e) {
      setRowErr(agentErrText(e));
    }
  };

  const openConv = async (a: AgentView) => {
    if (!s.conversations.some((c) => c.id === a.conversation_id)) {
      try {
        const conversations = await convs.list();
        setState({ conversations });
      } catch {
        /* 目录刷新失败时仍尝试打开，已在列表里的会话不受影响 */
      }
    }
    openConversation(a.conversation_id);
  };

  const menuButton = (id: string, label: string, items: { text: string; run: () => void }[]) => (
    <span className="am-menu">
      <button
        type="button"
        className="am-more"
        aria-label={`${label} 的更多操作`}
        aria-expanded={menu === id}
        onClick={(e) => {
          e.stopPropagation();
          setMenu(menu === id ? null : id);
        }}
      >
        ⋯
      </button>
      {menu === id && (
        <div className="am-menu-pop">
          {items.map((it) => (
            <button
              key={it.text}
              type="button"
              onClick={(e) => {
                e.stopPropagation();
                it.run();
              }}
            >
              {it.text}
            </button>
          ))}
        </div>
      )}
    </span>
  );

  const renameForm = (id: string, label: string, save: () => void) => (
    <form
      className="am-rename"
      onSubmit={(e) => {
        e.preventDefault();
        save();
      }}
    >
      <input
        aria-label={label}
        value={nameDraft}
        maxLength={60}
        autoFocus
        onChange={(e) => setNameDraft(e.target.value)}
      />
      <button type="submit" className="primary" disabled={rowBusy}>
        {rowBusy ? "保存中…" : "保存"}
      </button>
      <button type="button" disabled={rowBusy} onClick={() => setRenaming(null)}>
        取消
      </button>
    </form>
  );

  const sessionRow = (c: ConversationView) => {
    const on = view.kind === "session" && view.id === c.id;
    return (
      <div key={c.id}>
        <div className={`am-session${on ? " on" : ""}`}>
          <button type="button" className="am-session-main" onClick={() => requestNav({ kind: "session", id: c.id })}>
            <span className="am-session-title">{convTitle(c)}</span>
            <span className="am-session-meta">{c.agent?.name}</span>
          </button>
          {c.unread_count > 0 && <i className="badge am-badge">{c.unread_count > 99 ? "99+" : c.unread_count}</i>}
          {menuButton(c.id, convTitle(c), [
            { text: "改名", run: () => startRename(c.id, c.name ?? "") },
            {
              text: "删除",
              run: () => {
                setConfirmDel(c.id);
                setRowErr("");
                setMenu(null);
              },
            },
          ])}
        </div>
        {renaming === c.id && renameForm(c.id, "对话名称", () => void saveSessionName(c.id))}
        {confirmDel === c.id && (
          <div className="am-rename">
            <span className="am-hint">删除这条对话和它的全部消息？</span>
            <button type="button" disabled={rowBusy} onClick={() => setConfirmDel(null)}>
              取消
            </button>
            <button type="button" className="danger" disabled={rowBusy} onClick={() => void deleteSession(c.id)}>
              删除
            </button>
          </div>
        )}
        {(renaming === c.id || confirmDel === c.id) && rowErr && <div className="auth-err am-pad">{rowErr}</div>}
      </div>
    );
  };

  const current = view.kind === "agent" ? list.find((a) => a.id === view.id) ?? null : null;
  const newWs = view.kind === "new" ? workspaces.find((w) => w.id === view.wsId) ?? null : null;
  const inDetail = view.kind !== "blank";
  const isLocal = !!host && showLocalTag(local, host.id);

  return (
    <div className={`agent-page${inDetail ? " in-detail" : ""}`}>
      <aside className="am-list">
        <div className="am-list-head">
          <h2>Agent</h2>
        </div>
        {!loaded && <div className="am-hint am-pad">加载中…</div>}
        {loaded && hosts.length === 0 && (
          <div className="am-hint am-pad">还没有电脑。在桌面端登录后，这台电脑会自动出现。</div>
        )}
        {host && (
          <div className="am-host">
            <div className="am-row">
              <i className={`am-dot${host.online ? " on" : ""}`} />
              <select
                aria-label="电脑"
                value={host.id}
                onChange={(e) => {
                  setHostSel(e.target.value);
                  requestNav({ kind: "blank" });
                }}
              >
                {hosts.map((h) => (
                  <option key={h.id} value={h.id}>
                    {hostDisplayName(h)}
                    {showLocalTag(local, h.id) ? "（本机）" : ""}
                    {h.online ? "" : " · 离线"}
                  </option>
                ))}
              </select>
              {menuButton(`host:${host.id}`, hostDisplayName(host), [
                { text: "改名", run: () => startRename(`host:${host.id}`, host.label) },
              ])}
            </div>
            <div className="am-hint">
              {isLocal && <span className="am-tag">本机</span>}{" "}
              {hostGroupStatus(host.platform, host.online, fmtSeen(host.last_seen_at))}
            </div>
            {renaming === `host:${host.id}` && renameForm(host.id, "电脑名称", () => void saveHost(host.id))}
            {renaming === `host:${host.id}` && rowErr && <div className="auth-err am-pad">{rowErr}</div>}
          </div>
        )}
        <div className="am-list-body">
          {host && (
            <>
              <div className="am-sec">
                <span>工作区</span>
                <button
                  type="button"
                  className="am-more"
                  title="添加工作区"
                  aria-label="添加工作区"
                  onClick={() => {
                    setAddingWs(!addingWs);
                    setBrowsing(false);
                    setRowErr("");
                  }}
                >
                  ＋
                </button>
              </div>
              {addingWs && (
                <form
                  className="am-rename"
                  onSubmit={(e) => {
                    e.preventDefault();
                    void addWorkspace();
                  }}
                >
                  <input
                    aria-label="工作区路径"
                    placeholder={host.platform === "windows" ? "D:\\projects\\app" : "/home/me/app"}
                    value={wsPath}
                    maxLength={500}
                    autoFocus
                    onChange={(e) => setWsPath(e.target.value)}
                  />
                  <button type="button" disabled={rowBusy || !host.online} onClick={() => setBrowsing(!browsing)}>
                    浏览…
                  </button>
                  <button type="submit" className="primary" disabled={rowBusy || !wsPath.trim()}>
                    添加
                  </button>
                </form>
              )}
              {addingWs && !host.online && <div className="am-empty">电脑在线时才能浏览目录，也可以直接粘贴路径</div>}
              {addingWs && browsing && (
                <DirPicker
                  hostId={host.id}
                  onPick={(p) => void addWorkspace(p)}
                  onCancel={() => setBrowsing(false)}
                />
              )}
              {addingWs && rowErr && <div className="auth-err am-pad">{rowErr}</div>}
              {workspaces.every((w) => w.path === "") && !addingWs && (
                <div className="am-empty">点「＋」选这台电脑上的一个目录作为工作区。</div>
              )}
              {workspaces.map((ws) => {
                const sessions = workspaceSessions(s.conversations, ws.id);
                return (
                  <div className="am-ws" key={ws.id}>
                    <div className="am-ws-head" title={ws.path || "在电脑上 Chat 程序目录下的 tmp/<对话> 里干活"}>
                      <span className="am-ws-name">
                        {ws.path ? "📁" : "💬"} {folderName(ws.path)}
                      </span>
                      <button
                        type="button"
                        className={`am-more${view.kind === "new" && view.wsId === ws.id ? " on" : ""}`}
                        title="新对话"
                        aria-label={`在 ${folderName(ws.path)} 新开对话`}
                        onClick={() => requestNav({ kind: "new", wsId: ws.id })}
                      >
                        ＋
                      </button>
                      {ws.path !== "" &&
                        menuButton(`ws:${ws.id}`, folderName(ws.path), [
                          { text: "移除", run: () => void removeWorkspace(ws) },
                        ])}
                    </div>
                    {sessions.length === 0 && <div className="am-empty">还没有对话</div>}
                    {sessions.map(sessionRow)}
                  </div>
                );
              })}

              <div className="am-sec">
                <span>Agent</span>
                {allowAdd && isLocal && (
                  <button
                    type="button"
                    className="am-more"
                    title="添加 Agent"
                    aria-label="添加 Agent"
                    onClick={() => requestNav({ kind: "add" })}
                  >
                    ＋
                  </button>
                )}
              </div>
              {hostAgents.length === 0 && <div className="am-empty">{NO_AGENTS_ON_COMPUTER}</div>}
              {hostAgents.map((a) => {
                const on = view.kind === "agent" && view.id === a.id;
                const regId = a.runtime?.registry?.id;
                return (
                  <button
                    key={a.id}
                    type="button"
                    className={`am-agent${on ? " on" : ""}`}
                    onClick={() => requestNav({ kind: "agent", id: a.id })}
                  >
                    <AgentAvatar agentId={a.id} name={a.name} version={a.avatar_version} size="sm" />
                    <span className="am-agent-meta">
                      <b>{a.name}</b>
                      <span>{agentRowMeta(a.runtime, a.permission_tier, regId ? regNames[regId] : undefined)}</span>
                    </span>
                  </button>
                );
              })}
            </>
          )}
        </div>
        <div className="am-list-foot">
          <button
            type="button"
            className={view.kind === "defaults" ? "on" : ""}
            onClick={() => requestNav({ kind: "defaults" })}
          >
            ⚙ 默认设置
          </button>
          {!allowAdd && <div className="am-hint am-pad">{ADD_ON_DESKTOP_HINT}</div>}
        </div>
      </aside>

      <main className="am-detail">
        {pending && (
          <div className="am-discard" role="alertdialog" aria-label="放弃未保存的修改">
            <div className="am-discard-card">
              <p>有未保存的修改。要放弃这些修改吗？</p>
              <div className="am-row end">
                <button type="button" onClick={() => setPending(null)}>
                  继续编辑
                </button>
                <button
                  type="button"
                  className="primary"
                  onClick={() => {
                    setDirty(false);
                    go(pending);
                  }}
                >
                  放弃
                </button>
              </div>
            </div>
          </div>
        )}
        {view.kind === "session" && s.activeConv === view.id && (
          <section className="am-pane am-chat">
            <button type="button" className="am-back am-chat-back" onClick={() => requestNav({ kind: "blank" })}>
              ← 返回
            </button>
            <ChatView />
          </section>
        )}
        {view.kind === "new" && newWs && (
          <AgentSessionNew
            key={newWs.id}
            workspace={newWs}
            agents={hostAgents.filter((a) => !a.deleted)}
            regNames={regNames}
            onBack={() => requestNav({ kind: "blank" })}
          />
        )}
        {view.kind === "agent" && current && (
          <AgentDetail
            key={current.id}
            agent={current}
            host={hosts.find((h) => h.id === current.host_id) ?? null}
            local={showLocalTag(local, current.host_id)}
            settings={settings}
            notice={banner?.id === current.id ? banner.text : ""}
            onDirty={setDirty}
            onSaved={(a) => setList((cur) => cur.map((x) => (x.id === a.id ? a : x)))}
            onUpdated={(a) => setList((cur) => cur.map((x) => (x.id === a.id ? a : x)))}
            onDeleted={(id) => {
              setList((cur) => cur.filter((x) => x.id !== id));
              setDirty(false);
              setView({ kind: "blank" });
              setBanner({ id: "", text: "已删除" });
            }}
            onOpenConv={() => void openConv(current)}
            onBack={() => requestNav({ kind: "blank" })}
          />
        )}
        {view.kind === "agent" && !current && loaded && (
          <div className="am-blank">这个 Agent 已经不在列表里。</div>
        )}
        {view.kind === "add" && localHost && (
          <AgentAdd
            host={localHost}
            onCancel={() => requestNav({ kind: "blank" })}
            onCreated={(a) => {
              setList((cur) => [...cur, a]);
              setBanner({ id: a.id, text: "已创建，在工作区里点「＋」开个对话试试" });
              setDirty(false);
              setView({ kind: "agent", id: a.id });
            }}
          />
        )}
        {view.kind === "defaults" && (
          <section className="am-pane">
            <header className="am-head">
              <button type="button" className="am-back" onClick={() => requestNav({ kind: "blank" })}>
                ← 返回
              </button>
              <div className="am-head-text">
                <h2>默认设置</h2>
                <div className="am-sub">对你所有的 Agent 生效；单个 Agent 可在「回复」标签里改成别的。</div>
              </div>
            </header>
            <div className="am-scroll">
              <AgentSettings value={settings} onChange={setSettings} />
            </div>
          </section>
        )}
        {view.kind === "blank" && (
          <div className="am-blank">
            {banner && !banner.id ? banner.text : "选一个工作区开始对话，或者点它右边的「＋」新开一个。"}
          </div>
        )}
      </main>
    </div>
  );
}
