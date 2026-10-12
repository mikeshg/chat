import { useEffect, useState } from "react";
import { auth, loadSession, serverScope } from "./api";
import { readDrafts } from "./drafts";
import { cacheKey, clearRecent, readLocalCache, seedRecent } from "./localCache";
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
      clearRecent(); // MRU 是账号数据：别让下一个登录的用户继承
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
      } catch {
        // 凭据被明确拒绝时 api 层已清掉会话（loadSession 为空）→ 回登录页；
        // 其余一切失败（断网/超时/5xx/429/代理拦下的 4xx）都按离线进入：
        // 用本地缓存的身份与数据快照先进去，WS 自动重连，恢复后自动拉数据。
        const cached = loadSession()?.user;
        if (cached) {
          const snap = readLocalCache(localStorage, cacheKey(serverScope(), cached.id));
          if (snap) seedRecent(snap.recent);
          setState({
            me: cached,
            drafts: readDrafts(localStorage, cached.id),
            conversations: snap?.conversations ?? [],
            messages: snap?.messages ?? {},
          });
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
