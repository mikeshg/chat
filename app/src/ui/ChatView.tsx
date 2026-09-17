import { useEffect, useRef, useState } from "react";
import { convs } from "../api";
import { convTitle, getState, prependMessages, setState, useApp } from "../store";
import { sendChat, sendTyping } from "../ws";
import GroupInfo from "./GroupInfo";

export default function ChatView() {
  const s = useApp();
  const [text, setText] = useState("");
  const [showInfo, setShowInfo] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickBottom = useRef(true);
  const lastTypingSent = useRef(0);

  const conv = s.conversations.find((c) => c.id === s.activeConv) ?? null;
  const msgs = conv ? (s.messages[conv.id] ?? []) : [];
  const hasMore = conv ? (s.hasMore[conv.id] ?? false) : false;

  // 打开会话 → 拉历史（本地为空时）
  useEffect(() => {
    if (!conv) return;
    if ((getState().messages[conv.id] ?? []).length === 0) {
      convs
        .messages(conv.id, { limit: 50 })
        .then((p) =>
          setState((st) => ({
            messages: { ...st.messages, [conv.id]: p.messages },
            hasMore: { ...st.hasMore, [conv.id]: p.has_more },
          })),
        )
        .catch(console.warn);
    }
    setShowInfo(false);
  }, [conv?.id]);

  // 新消息滚到底（若本来就在底部）
  useEffect(() => {
    if (stickBottom.current && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [msgs.length, conv?.id]);

  if (!conv) {
    return (
      <main className="chat-view empty">
        <div className="empty-hint">选择一个会话开始聊天</div>
      </main>
    );
  }

  const title = convTitle(conv, s.me?.id);
  const peerOnline = conv.kind === "dm" && conv.peer ? !!s.online[conv.peer.id] : false;
  const typing = s.typing[conv.id];
  const typingFresh = typing && Date.now() - typing.ts < 5000;
  const readonly = conv.kind === "dm" && conv.peer && !s.friends.some((f) => f.user.id === conv.peer!.id);

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    stickBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
    if (el.scrollTop < 40 && hasMore && !loadingMore) loadMore();
  };

  const loadMore = async () => {
    if (!conv) return;
    const first = (getState().messages[conv.id] ?? [])[0];
    if (!first) return;
    setLoadingMore(true);
    try {
      const p = await convs.messages(conv.id, { before_seq: first.seq, limit: 50 });
      setState((st) => ({
        messages: {
          ...st.messages,
          [conv.id]: prependMessages(st.messages[conv.id] ?? [], p.messages),
        },
        hasMore: { ...st.hasMore, [conv.id]: p.has_more },
      }));
      // 保持视口不跳
      const el = scrollRef.current;
      if (el) requestAnimationFrame(() => (el.scrollTop += 1));
    } finally {
      setLoadingMore(false);
    }
  };

  const send = () => {
    const t = text.trim();
    if (!t || readonly) return;
    sendChat(conv.id, t);
    setText("");
    stickBottom.current = true;
  };

  const onInput = (v: string) => {
    setText(v);
    const now = Date.now();
    if (now - lastTypingSent.current > 2500) {
      lastTypingSent.current = now;
      sendTyping(conv.id);
    }
  };

  return (
    <main className="chat-view">
      <header className="chat-head">
        <div>
          <div className="chat-title">{title}</div>
          <div className="chat-status">
            {conv.kind === "dm"
              ? peerOnline
                ? "在线"
                : conv.peer?.last_seen_at
                  ? `最后在线 ${new Date(conv.peer.last_seen_at).toLocaleString()}`
                  : "离线"
              : `${conv.member_count} 名成员`}
          </div>
        </div>
        {conv.kind === "group" && (
          <button className="icon-btn" title="群信息" onClick={() => setShowInfo(!showInfo)}>
            ⋯
          </button>
        )}
      </header>

      <div className="msg-scroll" ref={scrollRef} onScroll={onScroll}>
        {loadingMore && <div className="msg-sys">加载历史…</div>}
        {!hasMore && msgs.length > 0 && <div className="msg-sys">— 没有更多了 —</div>}
        {msgs.map((m, i) => {
          const mine = m.sender_id === s.me?.id;
          const pending = m.id.startsWith("client:");
          const prev = msgs[i - 1];
          const showTime =
            !prev || new Date(m.created_at).getTime() - new Date(prev.created_at).getTime() > 300000;
          return (
            <div key={m.id}>
              {showTime && (
                <div className="msg-sys">{new Date(m.created_at).toLocaleString()}</div>
              )}
              <div className={`msg-row ${mine ? "mine" : ""}`}>
                {!mine && <div className="avatar sm">{m.sender_name[0]?.toUpperCase()}</div>}
                <div className="msg-body">
                  {!mine && conv.kind === "group" && (
                    <div className="msg-sender">{m.sender_name}</div>
                  )}
                  <div className={`bubble ${pending ? "pending" : ""}`}>{m.content}</div>
                </div>
              </div>
            </div>
          );
        })}
        {typingFresh && <div className="msg-sys">{typing.user.display_name} 正在输入…</div>}
      </div>

      <footer className="composer">
        {readonly ? (
          <div className="readonly-hint">对方已不是你的好友，会话只读</div>
        ) : (
          <>
            <textarea
              value={text}
              placeholder="输入消息，Enter 发送，Shift+Enter 换行"
              onChange={(e) => onInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  send();
                }
              }}
              rows={3}
            />
            <button className="primary send-btn" onClick={send} disabled={!text.trim()}>
              发送
            </button>
          </>
        )}
      </footer>
      {showInfo && conv.kind === "group" && (
        <GroupInfo conv={conv} onClose={() => setShowInfo(false)} />
      )}
    </main>
  );
}
