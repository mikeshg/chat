import { useEffect, useState } from "react";
import { auth, loadSession } from "./api";
import { readDrafts } from "./drafts";
import { syncWindowTitle, unreadTitle } from "./notify";
import { flushDrafts, setState, useApp } from "./store";
import { startWs, stopWs } from "./ws";
import AuthScreen from "./ui/AuthScreen";
import ChatScreen from "./ui/ChatScreen";

export default function App() {
  const s = useApp();
  const [booting, setBooting] = useState(!!loadSession());

  useEffect(() => {
    const onLogout = () => {
      flushDrafts(); // 先把在途草稿落盘（本地存储保留，同账号下次登录恢复）
      stopWs();
      setState({
        me: null,
        conversations: [],
        messages: {},
        hasMore: {},
        historyLoaded: {},
        friends: [],
        requests: { incoming: [], outgoing: [] },
        online: {},
        typing: {},
        activeConv: null,
        wsReady: false,
        toasts: [],
        drafts: {},
        avatars: {},
      });
    };
    window.addEventListener("chat:logout", onLogout);
    return () => window.removeEventListener("chat:logout", onLogout);
  }, []);

  useEffect(() => {
    if (!booting) return;
    // 有本地会话 → 校验 token 并拉初始数据
    (async () => {
      try {
        const me = await auth.me();
        setState({ me, drafts: readDrafts(localStorage, me.id) });
        void startWs();
      } catch {
        // session 已失效
      } finally {
        setBooting(false);
      }
    })();
  }, [booting]);

  // P1-1 标题未读数："(N) Chat"；登出后 conversations 清空 → 恢复 "Chat"
  useEffect(() => {
    syncWindowTitle(unreadTitle(s.conversations));
  }, [s.conversations]);

  if (booting) return <div className="boot">加载中…</div>;
  if (!s.me) return <AuthScreen />;
  return <ChatScreen />;
}
