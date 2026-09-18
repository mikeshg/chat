import { useEffect, useRef, useState } from "react";
import { convs, files } from "../api";
import { convTitle, getState, prependMessages, setState, useApp } from "../store";
import type { MemberView, MessageView } from "../types";
import { sendChat, sendTyping } from "../ws";
import GroupInfo from "./GroupInfo";

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/** 附件消息的 content 是服务端占位文本时不再重复渲染 */
function showText(m: MessageView): boolean {
  if (!m.attachment) return true;
  return m.content !== "[图片]" && m.content !== `[文件] ${m.attachment.file_name}`;
}

/** @username 片段高亮渲染 */
function renderContent(text: string) {
  return text.split(/(@[A-Za-z0-9_]+)/g).map((p, i) =>
    p.startsWith("@") ? (
      <span key={i} className="mt">
        {p}
      </span>
    ) : (
      p
    ),
  );
}

export default function ChatView() {
  const s = useApp();
  const [text, setText] = useState("");
  const [showInfo, setShowInfo] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickBottom = useRef(true);
  const lastTypingSent = useRef(0);
  const fileInput = useRef<HTMLInputElement>(null);
  const [uploading, setUploading] = useState(false);
  const taRef = useRef<HTMLTextAreaElement>(null);
  const [members, setMembers] = useState<MemberView[]>([]);
  const [ac, setAc] = useState<{ start: number; prefix: string } | null>(null);
  const [showSearch, setShowSearch] = useState(false);
  const [searchQ, setSearchQ] = useState("");
  const [searchRes, setSearchRes] = useState<MessageView[] | null>(null);
  const [flashId, setFlashId] = useState<string | null>(null);

  const conv = s.conversations.find((c) => c.id === s.activeConv) ?? null;
  const msgs = conv ? (s.messages[conv.id] ?? []) : [];
  const hasMore = conv ? (s.hasMore[conv.id] ?? false) : false;

  // 群聊拉成员列表（@补全用）
  useEffect(() => {
    if (conv?.kind === "group") {
      convs
        .detail(conv.id)
        .then((d) => setMembers(d.members))
        .catch(() => setMembers([]));
    } else {
      setMembers([]);
    }
    setAc(null);
    setShowSearch(false);
    setSearchRes(null);
    setFlashId(null);
  }, [conv?.id]);

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
    setAc(null);
    stickBottom.current = true;
  };

  // @补全候选：群成员中用户名前缀/昵称包含匹配，排除自己
  const acList = ac
    ? members
        .filter(
          (x) =>
            x.user.id !== s.me?.id &&
            (x.user.username.toLowerCase().startsWith(ac.prefix.toLowerCase()) ||
              x.user.display_name.toLowerCase().includes(ac.prefix.toLowerCase())),
        )
        .slice(0, 8)
    : [];

  const pickAc = (uname: string) => {
    if (!ac) return;
    const caret = taRef.current?.selectionStart ?? text.length;
    setText(text.slice(0, ac.start) + "@" + uname + " " + text.slice(caret));
    setAc(null);
    taRef.current?.focus();
  };

  const doSearch = (q: string) => {
    setSearchQ(q);
    if (!q.trim()) {
      setSearchRes(null);
      return;
    }
    convs
      .search(conv.id, q.trim())
      .then(setSearchRes)
      .catch(() => setSearchRes([]));
  };

  // 跳到搜索结果：目标消息不在本地时拉取以其为中心的窗口
  const jump = async (m: MessageView) => {
    setShowSearch(false);
    setFlashId(m.id);
    if (!(getState().messages[conv.id] ?? []).some((x) => x.id === m.id)) {
      try {
        const p = await convs.messages(conv.id, {
          after_seq: Math.max(0, m.seq - 25),
          limit: 50,
        });
        setState((st) => ({
          messages: { ...st.messages, [conv.id]: p.messages },
          hasMore: { ...st.hasMore, [conv.id]: (p.messages[0]?.seq ?? 1) > 1 },
        }));
      } catch (e) {
        console.warn(e);
      }
    }
    stickBottom.current = false;
    setTimeout(() => {
      scrollRef.current
        ?.querySelector(`[data-mid="${m.id}"]`)
        ?.scrollIntoView({ block: "center" });
    }, 60);
    setTimeout(() => setFlashId(null), 2600);
  };

  const onFile = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const f = e.target.files?.[0];
    e.target.value = "";
    if (!f || readonly) return;
    setUploading(true);
    try {
      const att = await files.upload(conv.id, f);
      sendChat(conv.id, "", att);
      stickBottom.current = true;
    } catch (err) {
      alert(err instanceof Error ? err.message : "上传失败");
    } finally {
      setUploading(false);
    }
  };

  const onInput = (v: string) => {
    setText(v);
    // @补全：光标前最近的 @ 前缀（群聊才启用）
    const caret = taRef.current?.selectionStart ?? v.length;
    const mm = /(^|[\s\n])@([A-Za-z0-9_]*)$/.exec(v.slice(0, caret));
    setAc(
      conv.kind === "group" && mm
        ? { start: caret - mm[2].length - 1, prefix: mm[2] }
        : null,
    );
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
        <button
          className="icon-btn"
          title="搜索聊天记录"
          onClick={() => {
            setShowSearch(!showSearch);
            setSearchQ("");
            setSearchRes(null);
          }}
        >
          🔍
        </button>
        {conv.kind === "group" && (
          <button className="icon-btn" title="群信息" onClick={() => setShowInfo(!showInfo)}>
            ⋯
          </button>
        )}
      </header>

      {showSearch && (
        <div className="search-panel">
          <input
            autoFocus
            placeholder="搜索聊天记录"
            value={searchQ}
            onChange={(e) => doSearch(e.target.value)}
          />
          <div className="search-res">
            {searchRes?.length === 0 && <div className="msg-sys">无结果</div>}
            {searchRes?.map((m) => (
              <div key={m.id} className="sr-item" onClick={() => jump(m)}>
                <span className="sr-sender">{m.sender_name}</span>
                <span className="sr-content">{m.content}</span>
                <span className="sr-time">
                  {new Date(m.created_at).toLocaleDateString()}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}

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
              <div
                className={`msg-row ${mine ? "mine" : ""}`}
                data-mid={m.id}
              >
                {!mine && <div className="avatar sm">{m.sender_name[0]?.toUpperCase()}</div>}
                <div className="msg-body">
                  {!mine && conv.kind === "group" && (
                    <div className="msg-sender">{m.sender_name}</div>
                  )}
                  <div
                    className={`bubble ${pending ? "pending" : ""} ${
                      m.mentions.includes(s.me?.id ?? "") ? "mentioned" : ""
                    } ${flashId === m.id ? "flash" : ""}`}
                  >
                    {m.attachment && m.kind === "image" && (
                      <img
                        className="msg-img"
                        src={files.url(m.attachment.id)}
                        alt={m.attachment.file_name}
                        onClick={() => window.open(files.url(m.attachment!.id))}
                      />
                    )}
                    {m.attachment && m.kind === "file" && (
                      <a
                        className="msg-file"
                        href={files.url(m.attachment.id)}
                        target="_blank"
                        rel="noreferrer"
                      >
                        <span className="file-ic">📄</span>
                        <span className="file-meta">
                          <b>{m.attachment.file_name}</b>
                          <i>{fmtSize(m.attachment.size)}</i>
                        </span>
                      </a>
                    )}
                    {showText(m) && renderContent(m.content)}
                  </div>
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
            <input ref={fileInput} type="file" hidden onChange={onFile} />
            <button
              className="icon-btn attach-btn"
              title="发送图片/文件"
              onClick={() => fileInput.current?.click()}
              disabled={uploading}
            >
              {uploading ? "…" : "📎"}
            </button>
            {acList.length > 0 && (
              <div className="ac-list">
                {acList.map((x) => (
                  <div
                    key={x.user.id}
                    className="ac-item"
                    onMouseDown={(e) => {
                      e.preventDefault();
                      pickAc(x.user.username);
                    }}
                  >
                    <span className="ac-name">@{x.user.username}</span>
                    <span className="ac-disp">{x.user.display_name}</span>
                  </div>
                ))}
              </div>
            )}
            <textarea
              ref={taRef}
              value={text}
              placeholder="输入消息，Enter 发送，Shift+Enter 换行"
              onChange={(e) => onInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  if (acList.length > 0) {
                    pickAc(acList[0].user.username);
                  } else {
                    send();
                  }
                }
                if (e.key === "Escape") setAc(null);
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
