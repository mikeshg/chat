import { useEffect, useState } from "react";
import { auth, clearSkipAutoLogin, hasSkipAutoLogin, loadSession, serverScope } from "./api";
import { readDrafts, writeDrafts } from "./drafts";
import { clearReedits, purgeReedits } from "./reedit";
import { cacheKey, clearRecent, readLocalCache, seedRecent } from "./localCache";
import { syncWindowTitle, unreadTitle } from "./notify";
import { flushDrafts, getState, setState, useApp } from "./store";
import { startWs, stopWs } from "./ws";
import AuthScreen from "./ui/AuthScreen";
import ChatScreen from "./ui/ChatScreen";
import { DialogHost, dismissAllDialogs } from "./ui/Modal";

export default function App() {
  const s = useApp();
  // 删了当前服务器的重载要停在登录页（标记到挂载后才消费，防初始化器重跑吃掉它）
  const [booting, setBooting] = useState(() => !hasSkipAutoLogin() && !!loadSession());
  useEffect(() => clearSkipAutoLogin(), []);

  useEffect(() => {
    const onLogout = () => {
      flushDrafts(); // 先把在途草稿落盘（本地存储保留，同账号下次登录恢复）
      // MM2：撤回暂存属敏感明文——re-edit 原文与草稿上的「复用附件」chip 随登出清掉；
      // 纯文字草稿维持原有语义（同账号下次登录恢复）
      const uid = getState().me?.id ?? loadSession()?.user.id;
      if (uid) {
        clearReedits(localStorage, serverScope(), uid);
        const dr = readDrafts(localStorage, serverScope(), uid);
        writeDrafts(
          localStorage,
          serverScope(),
          uid,
          Object.fromEntries(
            Object.entries(dr)
              .filter(([, e]) => e.text)
              .map(([k, e]) => [k, { text: e.text }]),
          ),
        );
      }
      clearRecent(); // MRU 是账号数据：别让下一个登录的用户继承
      dismissAllDialogs(); // 挂起的确认框按取消处理，别留到下一个会话
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
        purgeReedits(localStorage, serverScope(), me.id); // MM2：启动即清过期暂存
        setState({ me, drafts: readDrafts(localStorage, serverScope(), me.id) });
        void startWs();
      } catch {
        // 凭据被明确拒绝时 api 层已清掉会话（loadSession 为空）→ 回登录页；
        // 其余一切失败（断网/超时/5xx/429/代理拦下的 4xx）都按离线进入：
        // 用本地缓存的身份与数据快照先进去，WS 自动重连，恢复后自动拉数据。
        const cached = loadSession()?.user;
        if (cached) {
          purgeReedits(localStorage, serverScope(), cached.id); // MM2：启动即清过期暂存
          const snap = readLocalCache(localStorage, cacheKey(serverScope(), cached.id));
          if (snap) seedRecent(snap.recent);
          setState({
            me: cached,
            drafts: readDrafts(localStorage, serverScope(), cached.id),
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

  return (
    <>
      {booting ? <div className="boot">加载中…</div> : s.me ? <ChatScreen /> : <AuthScreen />}
      <DialogHost />
    </>
  );
}
