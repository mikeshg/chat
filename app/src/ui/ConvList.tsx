import { useMemo, useState } from "react";
import { convAvatarText, convTitle, setState, useApp } from "../store";
import { sendRead } from "../ws";
import NewGroupModal from "./NewGroupModal";

function fmtTime(iso?: string | null): string {
  if (!iso) return "";
  const d = new Date(iso);
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  if (sameDay) return d.toTimeString().slice(0, 5);
  const yesterday = new Date(now.getTime() - 86400000);
  if (d.toDateString() === yesterday.toDateString()) return "昨天";
  return `${d.getMonth() + 1}/${d.getDate()}`;
}

export default function ConvList() {
  const s = useApp();
  const [q, setQ] = useState("");
  const [showNewGroup, setShowNewGroup] = useState(false);

  const list = useMemo(() => {
    const kw = q.trim().toLowerCase();
    if (!kw) return s.conversations;
    return s.conversations.filter((c) =>
      convTitle(c).toLowerCase().includes(kw),
    );
  }, [s.conversations, q]);

  const open = (id: string) => {
    const c = s.conversations.find((x) => x.id === id);
    setState({ activeConv: id });
    // 打开即已读
    if (c && c.unread_count > 0 && c.last_seq > 0) {
      sendRead(id, c.last_seq);
      setState({
        conversations: s.conversations.map((x) =>
          x.id === id ? { ...x, unread_count: 0, has_mention: false } : x,
        ),
      });
    }
  };

  return (
    <aside className="conv-list">
      <div className="conv-head">
        <input
          className="search"
          placeholder="搜索"
          value={q}
          onChange={(e) => setQ(e.target.value)}
        />
        <button className="icon-btn" title="发起群聊" onClick={() => setShowNewGroup(true)}>
          +
        </button>
      </div>
      <div className="conv-scroll">
        {list.length === 0 && (
          <div className="empty-hint">
            还没有会话。
            <br />
            去「联系人」加好友开始聊天吧。
          </div>
        )}
        {list.map((c) => {
          const title = convTitle(c, s.me?.id);
          const peerOnline = c.kind === "dm" && c.peer ? !!s.online[c.peer.id] : false;
          const typing = s.typing[c.id];
          const typingFresh = typing && Date.now() - typing.ts < 5000;
          return (
            <div
              key={c.id}
              className={`conv-item ${s.activeConv === c.id ? "on" : ""}`}
              onClick={() => open(c.id)}
            >
              <div className="avatar">
                {convAvatarText(c)}
                {peerOnline && <i className="online-dot" />}
              </div>
              <div className="conv-mid">
                <div className="conv-title">{title}</div>
                <div className="conv-sub">
                  {c.has_mention && c.unread_count > 0 && (
                    <span className="mention-tag">[有人@我]</span>
                  )}
                  {typingFresh
                    ? "对方正在输入…"
                    : c.last_message
                      ? `${c.kind === "group" && c.last_message.sender_id !== s.me?.id ? c.last_message.sender_name + "：" : ""}${c.last_message.content}`
                      : "暂无消息"}
                </div>
              </div>
              <div className="conv-right">
                <div className="conv-time">{fmtTime(c.last_message?.created_at ?? c.created_at)}</div>
                {c.unread_count > 0 && (
                  <div className="unread">{c.unread_count > 99 ? "99+" : c.unread_count}</div>
                )}
              </div>
            </div>
          );
        })}
      </div>
      {showNewGroup && <NewGroupModal onClose={() => setShowNewGroup(false)} />}
    </aside>
  );
}
