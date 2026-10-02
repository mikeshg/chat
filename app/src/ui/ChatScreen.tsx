import { useEffect } from "react";
import { dismissToast, setState, useApp } from "../store";
import { openConversation, refreshCatalogs } from "../ws";
import ConvList from "./chat/ConvList";
import ChatView from "./chat/ChatView";
import ContactsPanel from "./ContactsPanel";
import MePanel from "./me/MePanel";
import AdminPanel from "./admin/AdminPanel";
import Avatar from "./Avatar";

export default function ChatScreen() {
  const s = useApp();

  useEffect(() => {
    void refreshCatalogs().catch((e) => console.warn("initial load failed", e));
  }, []);

  // 打开会话压一条历史记录：安卓返回键 / 浏览器后退先关会话回列表，而不是退出 app
  useEffect(() => {
    if (s.activeConv && !history.state?.chat) history.pushState({ chat: 1 }, "");
    else if (!s.activeConv && history.state?.chat) history.back();
  }, [s.activeConv]);
  useEffect(() => {
    const onPop = () => !history.state?.chat && setState({ activeConv: null });
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);

  const pendingCount = s.requests.incoming.length;
  const totalUnread = s.conversations.reduce((a, c) => a + c.unread_count, 0);

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
          onClick={() => setState({ tab: "chats" })}
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
