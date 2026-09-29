import { useEffect } from "react";
import { canUseAgent } from "../agentAccess";
import { dismissToast, isWorkspaceConv, setState, useApp } from "../store";
import { openConversation, refreshCatalogs } from "../ws";
import ConvList from "./ConvList";
import ChatView from "./ChatView";
import ContactsPanel from "./ContactsPanel";
import AgentManager from "./AgentManager";
import MePanel from "./MePanel";
import AdminPanel from "./AdminPanel";
import Avatar from "./Avatar";

export default function ChatScreen() {
  const s = useApp();

  useEffect(() => {
    void refreshCatalogs().catch((e) => console.warn("initial load failed", e));
  }, []);

  const pendingCount = s.requests.incoming.length;
  // A27：工作区对话的未读算在 Agent 按钮上
  const totalUnread = s.conversations.reduce((a, c) => a + (isWorkspaceConv(c) ? 0 : c.unread_count), 0);
  const agentUnread = s.conversations.reduce((a, c) => a + (isWorkspaceConv(c) ? c.unread_count : 0), 0);
  const activeInAgents = isWorkspaceConv(s.conversations.find((c) => c.id === s.activeConv));

  return (
    <div className="shell">
      <nav className="rail">
        <Avatar
          className="rail-avatar"
          userId={s.me?.id}
          name={s.me?.display_name}
          avatarVersion={s.me?.avatar_version}
          hasAvatar={s.me?.has_avatar}
          title={s.me?.display_name}
        />
        <button
          className={`rail-btn ${s.tab === "chats" ? "on" : ""}`}
          onClick={() => setState(activeInAgents ? { tab: "chats", activeConv: null } : { tab: "chats" })}
          title="聊天"
        >
          会话
          {totalUnread > 0 && <i className="badge">{totalUnread > 99 ? "99+" : totalUnread}</i>}
        </button>
        <button
          className={`rail-btn ${s.tab === "contacts" ? "on" : ""}`}
          onClick={() => setState({ tab: "contacts" })}
          title="联系人"
        >
          联系人
          {pendingCount > 0 && <i className="badge">{pendingCount}</i>}
        </button>
        {canUseAgent(s.me) && (
          <button
            className={`rail-btn ${s.tab === "agents" ? "on" : ""}`}
            onClick={() => setState({ tab: "agents" })}
            title="Agent"
          >
            Agent
            {agentUnread > 0 && <i className="badge">{agentUnread > 99 ? "99+" : agentUnread}</i>}
          </button>
        )}
        <button
          className={`rail-btn ${s.tab === "me" ? "on" : ""}`}
          onClick={() => setState({ tab: "me" })}
          title="我"
        >
          我
        </button>
        {s.me?.role === "admin" && (
          <button
            className={`rail-btn ${s.tab === "admin" ? "on" : ""}`}
            onClick={() => setState({ tab: "admin" })}
            title="后台管理"
          >
            管理
          </button>
        )}
        <div className="rail-foot">
          <span className={`dot ${s.wsReady ? "on" : ""}`} title={s.wsReady ? "已连接" : "未连接"} />
        </div>
      </nav>

      {s.tab === "chats" && (
        <>
          <ConvList />
          <ChatView />
        </>
      )}
      {s.tab === "contacts" && <ContactsPanel />}
      {s.tab === "agents" && canUseAgent(s.me) && <AgentManager />}
      {s.tab === "me" && <MePanel />}
      {s.tab === "admin" && s.me?.role === "admin" && <AdminPanel />}

      {/* 最小应用内提示：新消息 toast，点击打开会话；静音会话已被抑制（@我除外） */}
      <div className="toasts" aria-live="polite">
        {s.toasts.map((t) => (
          <button
            key={t.id}
            className="toast"
            onClick={() => {
              dismissToast(t.id);
              openConversation(t.convId);
            }}
          >
            <b>{t.title}</b>
            <span>{t.text}</span>
          </button>
        ))}
      </div>
    </div>
  );
}
