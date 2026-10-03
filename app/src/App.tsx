import { useEffect, useState } from "react";
import { ApiError, auth, loadSession } from "./api";
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
        pendingJump: null,
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
      } catch (e) {
        // 连不上服务器（不是凭据失效）：用本地缓存的身份先进去，WS 自己重连，待发消息照常显示
        const offline = !(e instanceof ApiError) || e.status >= 500;
        const cached = offline ? loadSession()?.user : undefined;
        if (cached) {
          setState({ me: cached, drafts: readDrafts(localStorage, cached.id) });
          void startWs();
        }
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
