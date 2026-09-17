import { useEffect } from "react";
import { convs, friends } from "../api";
import { setState, useApp } from "../store";
import ConvList from "./ConvList";
import ChatView from "./ChatView";
import ContactsPanel from "./ContactsPanel";
import MePanel from "./MePanel";

export default function ChatScreen() {
  const s = useApp();

  useEffect(() => {
    (async () => {
      try {
        const [c, f, r] = await Promise.all([
          convs.list(),
          friends.list(),
          friends.requests(),
        ]);
        setState({ conversations: c, friends: f, requests: r });
      } catch (e) {
        console.warn("initial load failed", e);
      }
    })();
  }, []);

  const pendingCount = s.requests.incoming.length;
  const totalUnread = s.conversations.reduce((a, c) => a + c.unread_count, 0);

  return (
    <div className="shell">
      <nav className="rail">
        <div className="rail-avatar" title={s.me?.display_name}>
          {(s.me?.display_name ?? "?")[0]?.toUpperCase()}
        </div>
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
    </div>
  );
}
