import { useEffect } from "react";
import { dismissToast, setState, useApp } from "../store";
import { consumeBack } from "./chat/backStack";
import { revealDownloaded } from "../download";
import { openConversation, refreshCatalogs } from "../ws";
import ConvList from "./chat/ConvList";
import ChatView from "./chat/ChatView";
import ContactsPanel from "./ContactsPanel";
import MePanel from "./me/MePanel";
import AdminPanel from "./admin/AdminPanel";
import Avatar from "./Avatar";
import { ChatBubbleIcon, ContactsIcon, GearIcon, ShieldIcon } from "./icons";

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
    const onPop = () => {
      if (history.state?.chat) return;
      // MF2：返回键先关 ChatView 里最上层的覆盖物（转发弹窗 → 预览 → 记录查看页 → 多选），
      // 消费掉就补回 {chat:1} 让会话保持打开；没有覆盖层才退回会话列表
      if (consumeBack()) history.pushState({ chat: 1 }, "");
      else setState({ activeConv: null });
    };
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
          data-testid="rail-chats"
        >
          <ChatBubbleIcon />
          会话
          {totalUnread > 0 && <i className="badge">{totalUnread > 99 ? "99+" : totalUnread}</i>}
        </button>
        <button
          className={`rail-btn ${s.tab === "contacts" ? "on" : ""}`}
          onClick={() => setState({ tab: "contacts" })}
          title="联系人"
          data-testid="rail-contacts"
        >
          <ContactsIcon />
          联系人
          {pendingCount > 0 && <i className="badge">{pendingCount}</i>}
        </button>
        <button
          className={`rail-btn ${s.tab === "me" ? "on" : ""}`}
          onClick={() => setState({ tab: "me" })}
          title="我"
          data-testid="rail-me"
        >
          <GearIcon />
          我
        </button>
        {s.me?.role === "admin" && (
          <button
            className={`rail-btn ${s.tab === "admin" ? "on" : ""}`}
            onClick={() => setState({ tab: "admin" })}
            title="后台管理"
            data-testid="rail-admin"
          >
            <ShieldIcon />
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

      {/* 最小应用内提示：新消息 toast 点击打开会话；下载完成 toast（桌面）点击在文件夹中显示 */}
      <div className="toasts" aria-live="polite">
        {s.toasts.map((t) => (
          <button
            key={t.id}
            className="toast"
            onClick={() => {
              dismissToast(t.id);
              if (t.filePath) void revealDownloaded(t.filePath);
              else if (t.convId) openConversation(t.convId);
            }}
          >
            <b>{t.title}</b>
            {t.text && <span>{t.text}</span>}
            {/* MF2：结果未确认类的操作重试（同一 client_op_id 幂等重发） */}
            {t.action && (
              <span
                className="toast-act"
                role="button"
                onClick={(e) => {
                  e.stopPropagation();
                  dismissToast(t.id);
                  t.action!.run();
                }}
              >
                {t.action.label}
              </span>
            )}
          </button>
        ))}
      </div>
    </div>
  );
}
