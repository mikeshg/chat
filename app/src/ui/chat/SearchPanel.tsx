import { useEffect, useMemo, useRef, useState } from "react";
import { searchApi } from "../../api";
import type { SearchParams } from "../../api";
import { setState, useApp } from "../../store";
import type { MessageView } from "../../types";
import { openConversation } from "../../ws";
import Avatar from "../Avatar";
import {
  convTitleOf,
  highlightParts,
  matchConvsByTitle,
  senderOptions,
  snippetOf,
} from "./searchSnippets";
import { fmtTime } from "./fmtTime";

// ===== V2 P3-1 =====
// 会话列表搜索态：上半「会话」本地过滤、下半「聊天记录」走 /v1/search（300ms 防抖）。
// 类型 chip + 更多筛选（发送者/日期）作用于聊天记录；滚到底用 before 游标翻页。

type KindFilter = "" | "image" | "file" | "audio";


/** <input type="date"> 的本地日期 → RFC3339 界：from 当天零点（含），to 次日零点（不含） */
function dayStart(d: string): string | undefined {
  const t = new Date(`${d}T00:00:00`);
  return Number.isNaN(t.getTime()) ? undefined : t.toISOString();
}
function dayAfter(d: string): string | undefined {
  const t = new Date(`${d}T00:00:00`);
  if (Number.isNaN(t.getTime())) return undefined;
  return new Date(t.getTime() + 86400000).toISOString();
}

export default function SearchPanel({ q }: { q: string }) {
  const s = useApp();
  const [kind, setKind] = useState<KindFilter>("");
  const [moreOpen, setMoreOpen] = useState(false);
  const [senderId, setSenderId] = useState("");
  const [fromD, setFromD] = useState("");
  const [toD, setToD] = useState("");
  const [res, setRes] = useState<MessageView[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const reqRef = useRef(0);
  const scrollRef = useRef<HTMLDivElement>(null);

  // 上半部分「会话」：本地标题过滤即时更新（不走服务端）
  const convMatches = useMemo(
    () => matchConvsByTitle(s.conversations, q, s.me?.id),
    [s.conversations, q, s.me?.id],
  );
  // 「更多筛选」发送者候选：好友 + 会话目录 peer + 已缓存消息里出现过的人
  const senders = useMemo(
    () => senderOptions(s.friends, s.conversations, s.messages),
    [s.friends, s.conversations, s.messages],
  );

  const buildParams = (page?: { before: string; before_id: string }): SearchParams => ({
    q: q.trim() || undefined,
    kind: kind || undefined,
    sender_id: senderId || undefined,
    from: fromD ? dayStart(fromD) : undefined,
    to: toD ? dayAfter(toD) : undefined,
    limit: 30,
    ...page,
  });

  // 下半部分「聊天记录」：输入/筛选变化防抖 300ms 重新查第一页
  useEffect(() => {
    const req = ++reqRef.current;
    setLoading(true);
    setErr(null);
    const t = setTimeout(() => {
      searchApi
        .messages(buildParams())
        .then((p) => {
          if (req !== reqRef.current) return;
          setRes(p.messages);
          setHasMore(p.has_more);
          scrollRef.current?.scrollTo({ top: 0 });
        })
        .catch((e) => {
          if (req !== reqRef.current) return;
          setRes([]);
          setHasMore(false);
          setErr(e instanceof Error ? e.message : "搜索失败");
        })
        .finally(() => {
          if (req === reqRef.current) setLoading(false);
        });
    }, 300);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [q, kind, senderId, fromD, toD]);

  const loadMore = () => {
    const last = res[res.length - 1];
    if (!last || !hasMore || loading) return;
    const req = ++reqRef.current;
    setLoading(true);
    searchApi
      .messages(buildParams({ before: last.created_at, before_id: last.id }))
      .then((p) => {
        if (req !== reqRef.current) return;
        setRes((cur) => {
          const seen = new Set(cur.map((m) => m.id));
          return [...cur, ...p.messages.filter((m) => !seen.has(m.id))];
        });
        setHasMore(p.has_more);
      })
      .catch(() => {})
      .finally(() => {
        if (req === reqRef.current) setLoading(false);
      });
  };

  const onScroll = () => {
    const el = scrollRef.current;
    if (el && el.scrollHeight - el.scrollTop - el.clientHeight < 30) loadMore();
  };

  // 点结果：打开会话并把定位任务交给 ChatView（pendingJump 消费后清空）
  const open = (m: MessageView) => {
    setState({
      pendingJump: { convId: m.conversation_id, messageId: m.id, seq: m.seq },
    });
    openConversation(m.conversation_id);
  };

  const kindChips: { v: KindFilter; label: string }[] = [
    { v: "", label: "全部" },
    { v: "image", label: "图片" },
    { v: "file", label: "文件" },
    { v: "audio", label: "语音" },
  ];

  return (
    <div className="conv-scroll search-mode" ref={scrollRef} onScroll={onScroll}>
      <div className="gs-chips">
        {kindChips.map((c) => (
          <button
            key={c.label}
            className={`gs-chip ${kind === c.v ? "on" : ""}`}
            onClick={() => setKind(c.v)}
          >
            {c.label}
          </button>
        ))}
        <button
          className={`gs-chip ${moreOpen || senderId || fromD || toD ? "on" : ""}`}
          onClick={() => setMoreOpen(!moreOpen)}
        >
          更多筛选
        </button>
      </div>
      {moreOpen && (
        <div className="gs-filters">
          <select
            value={senderId}
            onChange={(e) => setSenderId(e.target.value)}
            aria-label="发送者"
          >
            <option value="">全部发送者</option>
            {senders.map((u) => (
              <option key={u.id} value={u.id}>
                {u.display_name}
              </option>
            ))}
          </select>
          <input
            type="date"
            value={fromD}
            onChange={(e) => setFromD(e.target.value)}
            aria-label="开始日期"
          />
          <span className="muted">至</span>
          <input
            type="date"
            value={toD}
            onChange={(e) => setToD(e.target.value)}
            aria-label="结束日期"
          />
        </div>
      )}

      {convMatches.length > 0 && (
        <div className="gs-sec">
          <div className="gs-sec-title">会话</div>
          {convMatches.map((c) => (
            <div
              key={c.id}
              className="conv-item"
              onClick={() => openConversation(c.id)}
            >
              {c.kind === "dm" && c.peer ? (
                <Avatar
                  userId={c.peer.id}
                  name={c.peer.display_name}
                  avatarVersion={c.peer.avatar_version}
                  hasAvatar={c.peer.has_avatar}
                />
              ) : (
                <div className={`avatar ${c.kind === "self" ? "self-avatar" : ""}`}>
                  {c.kind === "self" ? "🗂" : convTitleOf([c], c.id, s.me?.id)[0]}
                </div>
              )}
              <div className="conv-mid">
                <div className="conv-title">
                  {convTitleOf(s.conversations, c.id, s.me?.id)}
                </div>
              </div>
            </div>
          ))}
        </div>
      )}

      <div className="gs-sec">
        <div className="gs-sec-title">聊天记录</div>
        {err && <div className="ctx-err">{err}</div>}
        {res.map((m) => (
          <div key={m.id} className="sr-item" onClick={() => open(m)}>
            <div className="gs-item-main">
              <div className="gs-item-top">
                <span className="gs-conv">
                  {convTitleOf(s.conversations, m.conversation_id, s.me?.id)}
                </span>
                <span className="sr-sender">{m.sender_name}</span>
                <span className="sr-time">{fmtTime(m.created_at)}</span>
              </div>
              <div className="sr-content">
                {highlightParts(snippetOf(m), q).map((p, i) =>
                  p.hit ? <mark key={i}>{p.text}</mark> : <span key={i}>{p.text}</span>,
                )}
              </div>
            </div>
          </div>
        ))}
        {!loading && !err && res.length === 0 && (
          <div className="gs-empty">没有匹配的聊天记录</div>
        )}
        {loading && <div className="msg-sys">搜索中…</div>}
        {!loading && hasMore && <div className="msg-sys">下拉加载更多</div>}
      </div>
    </div>
  );
}
