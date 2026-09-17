import { useEffect, useState } from "react";
import { auth, loadSession } from "./api";
import { setState, useApp } from "./store";
import { startWs, stopWs } from "./ws";
import AuthScreen from "./ui/AuthScreen";
import ChatScreen from "./ui/ChatScreen";

export default function App() {
  const s = useApp();
  const [booting, setBooting] = useState(!!loadSession());

  useEffect(() => {
    const onLogout = () => {
      stopWs();
      setState({
        me: null,
        conversations: [],
        messages: {},
        friends: [],
        requests: { incoming: [], outgoing: [] },
        online: {},
        typing: {},
        activeConv: null,
        wsReady: false,
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
        setState({ me });
        void startWs();
      } catch {
        // session 已失效
      } finally {
        setBooting(false);
      }
    })();
  }, [booting]);

  if (booting) return <div className="boot">加载中…</div>;
  if (!s.me) return <AuthScreen />;
  return <ChatScreen />;
}
