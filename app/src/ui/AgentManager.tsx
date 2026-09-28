import { useEffect, useState } from "react";
import { isDesktopShell } from "../agentHost";
import { agents, convs } from "../api";
import { setState, useApp } from "../store";
import type { AgentUserSettings, AgentView } from "../types";
import { openConversation } from "../ws";
import { groupAgentsByHost, hostDisplayName } from "./agentHosts";
import {
  ADD_ON_DESKTOP_HINT,
  NO_AGENTS_ON_COMPUTER,
  agentErrText,
  agentRowMeta,
  canAddAgent,
  hostGroupStatus,
  readInjectedLocalHostId,
  resolveLocalComputer,
  showLocalTag,
} from "./agentPage";
import AgentAdd from "./AgentAdd";
import AgentAvatar from "./AgentAvatar";
import AgentDetail from "./AgentDetail";
import AgentSettings from "./AgentSettings";

type Pane = { kind: "blank" } | { kind: "agent"; id: string } | { kind: "add" } | { kind: "defaults" };

function fmtSeen(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const m = d.getMonth() + 1;
  const day = d.getDate();
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return `${m}/${day} ${hh}:${mm}`;
}

/** Agent 管理页：左栏按电脑分组，右栏是详情、添加或默认设置。 */
export default function AgentManager() {
  const s = useApp();
  const [list, setList] = useState<AgentView[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [view, setView] = useState<Pane>({ kind: "blank" });
  const [pending, setPending] = useState<Pane | null>(null);
  const [dirty, setDirty] = useState(false);
  const [menu, setMenu] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [hostDraft, setHostDraft] = useState("");
  const [hostErr, setHostErr] = useState("");
  const [hostBusy, setHostBusy] = useState(false);
  const [settings, setSettings] = useState<AgentUserSettings | null>(null);
  const [names, setNames] = useState<Record<string, string>>({});
  const [banner, setBanner] = useState<{ id: string; text: string } | null>(null);

  const local = resolveLocalComputer({
    desktopShell: isDesktopShell(),
    storeHostId: s.localHostId,
    injectedHostId: readInjectedLocalHostId(),
  });
  const hosts = s.agentHosts;
  const groups = groupAgentsByHost(hosts, list);
  const allowAdd = canAddAgent(local, hosts);
  const localHost = hosts.find((h) => h.id === local.localHostId) ?? null;
  const hostKey = hosts.map((h) => `${h.id}:${h.online}`).join("|");

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

  const requestNav = (next: Pane) => {
    const leaving =
      view.kind === "agent" && !(next.kind === "agent" && next.id === view.id);
    if (dirty && leaving) {
      setPending(next);
      return;
    }
    setPending(null);
    setView(next);
    setMenu(null);
  };

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
      setRenaming(null);
    } catch (e) {
      setHostErr(agentErrText(e));
    } finally {
      setHostBusy(false);
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

  const current = view.kind === "agent" ? list.find((a) => a.id === view.id) ?? null : null;
  const inDetail = view.kind !== "blank";

  return (
    <div className={`agent-page${inDetail ? " in-detail" : ""}`}>
      <aside className="am-list">
        <div className="am-list-head">
          <h2>Agent</h2>
          {allowAdd && (
            <button type="button" className="primary" onClick={() => requestNav({ kind: "add" })}>
              ＋ 添加
            </button>
          )}
        </div>
        <div className="am-list-body">
          {!loaded && <div className="am-hint am-pad">加载中…</div>}
          {loaded && groups.length === 0 && (
            <div className="am-hint am-pad">还没有电脑。在桌面端登录后，这台电脑会自动出现。</div>
          )}
          {groups.map((g) => {
            const host = g.host;
            const id = host?.id ?? "unknown";
            const title = host ? hostDisplayName(host) : "未知电脑";
            return (
              <div className="am-pc" key={id}>
                <div className="am-pc-head">
                  <i className={`am-dot${host?.online ? " on" : ""}`} />
                  <span className="am-pc-name">
                    <b>{title}</b>
                    {host && showLocalTag(local, host.id) && <span className="am-tag">本机</span>}
                    {host && (
                      <span>{hostGroupStatus(host.platform, host.online, fmtSeen(host.last_seen_at))}</span>
                    )}
                  </span>
                  {host && (
                    <span className="am-menu">
                      <button
                        type="button"
                        className="am-more"
                        aria-label={`${title} 的更多操作`}
                        aria-expanded={menu === host.id}
                        onClick={(e) => {
                          e.stopPropagation();
                          setMenu(menu === host.id ? null : host.id);
                        }}
                      >
                        ⋯
                      </button>
                      {menu === host.id && (
                        <div className="am-menu-pop">
                          <button
                            type="button"
                            onClick={(e) => {
                              e.stopPropagation();
                              setRenaming(host.id);
                              setHostDraft(host.label);
                              setHostErr("");
                              setMenu(null);
                            }}
                          >
                            改名
                          </button>
                        </div>
                      )}
                    </span>
                  )}
                </div>
                {host && renaming === host.id && (
                  <form
                    className="am-rename"
                    onSubmit={(e) => {
                      e.preventDefault();
                      void saveHost(host.id);
                    }}
                  >
                    <input
                      aria-label="电脑名称"
                      value={hostDraft}
                      maxLength={60}
                      autoFocus
                      onChange={(e) => setHostDraft(e.target.value)}
                    />
                    <button type="submit" className="primary" disabled={hostBusy}>
                      {hostBusy ? "保存中…" : "保存"}
                    </button>
                    <button
                      type="button"
                      disabled={hostBusy}
                      onClick={() => {
                        setRenaming(null);
                        setHostErr("");
                      }}
                    >
                      取消
                    </button>
                  </form>
                )}
                {host && renaming === host.id && hostErr && <div className="auth-err am-pad">{hostErr}</div>}
                {g.agents.length === 0 && (
                  <div className="am-empty">
                    {NO_AGENTS_ON_COMPUTER}
                    {host && showLocalTag(local, host.id) && allowAdd ? "，点右上角「添加」" : ""}
                  </div>
                )}
                {g.agents.map((a) => {
                  const on = view.kind === "agent" && view.id === a.id;
                  const regId = a.runtime?.registry?.id;
                  const regName = regId ? names[`${a.host_id}:${regId}`] : undefined;
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
                        <span>{agentRowMeta(a.runtime, a.permission_tier, regName)}</span>
                      </span>
                      <i
                        className={`am-dot${host?.online ? " on" : ""}`}
                        title={host?.online ? "电脑在线" : "电脑离线，调用会排队"}
                      />
                    </button>
                  );
                })}
              </div>
            );
          })}
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
                    setView(pending);
                    setPending(null);
                  }}
                >
                  放弃
                </button>
              </div>
            </div>
          </div>
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
              setBanner({ id: a.id, text: "已创建，去专属会话发条消息试试" });
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
        {view.kind === "blank" && <div className="am-blank">从左边选一个 Agent 查看和修改设置。</div>}
      </main>
    </div>
  );
}
