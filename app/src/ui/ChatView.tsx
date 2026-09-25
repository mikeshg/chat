import { useEffect, useRef, useState } from "react";
import { ApiError, convs, files, msgs as msgsApi, session } from "../api";
import {
  applyRefreshed,
  applyUpdatedMessage,
  convTitle,
  getState,
  prependMessages,
  setDraft,
  setState,
  upsertMessage,
  useApp,
} from "../store";
import type { AttachmentView, MemberView, MessageView, ReplyPreview } from "../types";
import { sendChat, sendTyping } from "../ws";
import RichText from "../richText";
import GroupInfo from "./GroupInfo";
import NewPollModal from "./NewPollModal";
import PollCard from "./PollCard";

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

/** 可按文本预览的附件（mime 或扩展名判断） */
function isTextLike(a: AttachmentView): boolean {
  return (
    a.mime.startsWith("text/") ||
    /\.(txt|md|log|json|js|jsx|ts|tsx|py|rs|csv|ya?ml|xml|html?|css|sh|bat|ps1|c|h|cpp|java|go|toml|ini|cfg|sql)$/i.test(
      a.file_name,
    )
  );
}

/** 正在拉取初始历史页的会话（避免 StrictMode 双挂载/快速切换重复请求） */
const historyInflight = new Set<string>();

type EditSubmission = { id: string; content: string; expectedVersion: number; clientOpId: string };
export function prepareEditSubmission(previous: EditSubmission | null, id: string, text: string, expectedVersion: number): EditSubmission {
  const content = text.trim();
  if (!content || Array.from(content).length > 4000) throw new Error("消息需 1–4000 字");
  if (previous?.id === id && previous.content === content) return previous;
  return { id, content, expectedVersion, clientOpId: crypto.randomUUID() };
}

/** 编辑结果归属：仅当组件 owner、编辑代次、目标消息、会话与账号代次全部未变时才收尾编辑态 */
export function ownsEditResult(
  submitted: { token: object | null; rev: number; messageId: string; convId: string; gen: number },
  live: { owner: object | null; rev: number; editingId: string | null; activeConv: string | null; gen: number },
): boolean {
  return (
    submitted.token != null &&
    submitted.token === live.owner &&
    submitted.rev === live.rev &&
    live.editingId === submitted.messageId &&
    submitted.gen === live.gen &&
    live.activeConv === submitted.convId
  );
}

/** 服务端明确拒绝的编辑结果：下一次提交必须是新 op；其余错误按“结果未确认”保留原 clientOpId 供幂等重试 */
export function editRejected(e: unknown): boolean {
  return e instanceof ApiError && [400, 403, 404, 409, 422].includes(e.status);
}

/** 快捷 emoji 表态集合 */
const QUICK_EMOJI = ["👍", "❤️", "😂", "😮", "😢", "🙏", "🎉", "🔥"];

/** 回复预览里非文本 kind 的占位摘要 */
function kindLabel(kind: string): string {
  if (kind === "image") return "[图片]";
  if (kind === "file") return "[文件]";
  if (kind === "poll") return "[投票]";
  return "";
}

/** 编辑窗口 15 分钟（客户端只用于隐藏入口，服务端按库时间强制判定） */
function withinEditWindow(m: MessageView): boolean {
  return Date.now() - new Date(m.created_at).getTime() < 15 * 60 * 1000;
}

/** 触发客户端下载（借助 Content-Disposition: attachment） */
function downloadAttachment(a: AttachmentView) {
  const el = document.createElement("a");
  el.href = files.url(a.id, true);
  el.download = a.file_name;
  document.body.appendChild(el);
  el.click();
  el.remove();
}

export default function ChatView() {
  const s = useApp();
  // 输入框初值取当前会话草稿（组件重挂时不再闪空）
  const [text, setText] = useState(() => {
    const st = getState();
    return st.activeConv ? (st.drafts[st.activeConv] ?? "") : "";
  });
  const [showInfo, setShowInfo] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickBottom = useRef(true);
  const lastTypingSent = useRef(0);
  const fileInput = useRef<HTMLInputElement>(null);
  const taRef = useRef<HTMLTextAreaElement>(null);
  const draftBeforeEdit = useRef(""); // 编辑态接管 composer 前的草稿
  const prevConv = useRef<string | null>(null); // 上一次渲染的会话（草稿归属判定）
  const [members, setMembers] = useState<MemberView[]>([]);
  const [ac, setAc] = useState<{ start: number; prefix: string } | null>(null);
  const [showSearch, setShowSearch] = useState(false);
  const [searchQ, setSearchQ] = useState("");
  const [searchRes, setSearchRes] = useState<MessageView[] | null>(null);
  const [flashId, setFlashId] = useState<string | null>(null);
  const [preview, setPreview] = useState<AttachmentView | null>(null);
  const [previewText, setPreviewText] = useState<string | null>(null);
  const [ctxMenu, setCtxMenu] = useState<{
    x: number;
    y: number;
    att: AttachmentView;
  } | null>(null);
  const [pendingFiles, setPendingFiles] = useState<File[]>([]);
  const [sending, setSending] = useState(false);
  // 生命周期：emoji 选择器展开的消息 id、消息右键菜单、mutation 错误/忙碌
  const [emojiPick, setEmojiPick] = useState<string | null>(null);
  const [msgMenu, setMsgMenu] = useState<{ x: number; y: number; m: MessageView } | null>(null);
  const [mutErr, setMutErr] = useState<string | null>(null);
  const [mutBusy, setMutBusy] = useState(false);
  // 投票：创建弹窗开关 + 每条消息的操作 busy 态
  const [showPoll, setShowPoll] = useState(false);
  const [pollBusy, setPollBusy] = useState<Record<string, boolean>>({});
  const gen = session();
  const viewOwner = useRef<object | null>(null);
  const mutation = useRef<object | null>(null);
  const editSubmission = useRef<EditSubmission | null>(null);
  const draftRevision = useRef(0);
  const uploadOwner = useRef<object | null>(null);
  const moreOwner = useRef<object | null>(null);
  const searchRequest = useRef(0);
  const previewRequest = useRef(0);
  const jumpRequest = useRef(0);
  const jumpTimers = useRef<ReturnType<typeof setTimeout>[]>([]);

  const conv = s.conversations.find((c) => c.id === s.activeConv) ?? null;
  const msgs = conv ? (s.messages[conv.id] ?? []) : [];
  const hasMore = conv ? (s.hasMore[conv.id] ?? false) : false;
  const current = (token: object | null) => token != null && token === viewOwner.current && gen === session() && getState().activeConv === conv?.id;

  // 群聊拉成员列表（@补全用）
  useEffect(() => {
    const token = {};
    viewOwner.current = token;
    setMembers([]);
    if (conv?.kind === "group") {
      convs
        .detail(conv.id)
        .then((d) => { if (current(token)) setMembers(d.members); })
        .catch(() => { if (current(token)) setMembers([]); });
    }
    setAc(null);
    setShowSearch(false);
    setSearchRes(null);
    setFlashId(null);
    setEmojiPick(null);
    setMsgMenu(null);
    setMutErr(null);
    // 切换会话：在途 mutation 回调失权（owner token 已换），忙碌态与编辑代次一并复位
    mutation.current = null;
    draftRevision.current++;
    setMutBusy(false);
    // V2 P1-3：旧会话草稿落库。编辑态中输入框是被编辑内容，真草稿在 draftBeforeEdit
    if (prevConv.current != null && prevConv.current !== conv?.id) {
      setDraft(prevConv.current, getState().editingMessage ? draftBeforeEdit.current : text);
      draftBeforeEdit.current = "";
    }
    prevConv.current = conv?.id ?? null;
    // 切换会话时退出回复/编辑态（目标属于旧会话）
    setState({ replyingTo: null, editingMessage: null });
    setText(conv ? (getState().drafts[conv.id] ?? "") : "");
  }, [conv?.id]);

  // 打开会话 → 拉历史（P1-11：每会话至少一次；实时缓存非空也要拉，按 id 合并不覆盖）
  useEffect(() => {
    if (!conv) return;
    setShowInfo(false);
    if (getState().historyLoaded[conv.id] || historyInflight.has(conv.id)) return;
    historyInflight.add(conv.id);
    convs
      .messages(conv.id, { limit: 50 })
      .then((p) =>
        setState((st) => {
          const merged = p.messages.reduce(
            (l, m) => upsertMessage(l, m),
            st.messages[conv.id] ?? [],
          );
          return {
            messages: { ...st.messages, [conv.id]: merged },
            hasMore: { ...st.hasMore, [conv.id]: (merged[0]?.seq ?? 1) > 1 },
            historyLoaded: { ...st.historyLoaded, [conv.id]: true },
          };
        }),
      )
      .catch(console.warn)
      .finally(() => historyInflight.delete(conv.id));
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

  // 生命周期入口可见性（服务端在锁内二次校验；这里只做 UI 层收敛）
  const canEditMsg = (m: MessageView) =>
    !m.id.startsWith("client:") &&
    !m.deleted_at &&
    m.sender_id === s.me?.id &&
    m.kind === "text" &&
    withinEditWindow(m);
  const canDeleteMsg = (m: MessageView) =>
    !m.id.startsWith("client:") &&
    !m.deleted_at &&
    (m.sender_id === s.me?.id || (conv.kind === "group" && conv.owner_id === s.me?.id));
  const canActMsg = (m: MessageView) =>
    !m.id.startsWith("client:") && !m.deleted_at && !readonly;

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

  // ---------- 生命周期动作 ----------

  const startReply = (m: MessageView) => {
    setState({ replyingTo: m, editingMessage: null });
    setMutErr(null);
    taRef.current?.focus();
  };

  const startEdit = (m: MessageView) => {
    draftRevision.current++; // 新编辑态：在途的旧编辑回调一律作废
    draftBeforeEdit.current = text; // 暂存草稿，取消/保存后还原
    setState({ editingMessage: m, replyingTo: null });
    setText(m.content);
    setMutErr(null);
    taRef.current?.focus();
  };

  const cancelEdit = () => {
    draftRevision.current++;
    setState({ editingMessage: null });
    setText(draftBeforeEdit.current);
    draftBeforeEdit.current = "";
    setMutErr(null);
  };

  /** 编辑提交：REST + expected_version；结果未确认的重试复用同一 client_op_id 幂等回放，
   * 409 说明期间被别人（另一设备）改过。回调先判归属：取消/改编辑另一条/换会话或账号后不碰编辑态与草稿。 */
  const submitEdit = async () => {
    const em = s.editingMessage;
    const t = text.trim();
    const token = viewOwner.current;
    const rev = draftRevision.current;
    if (!em || !t || mutBusy || mutation.current != null || !current(token)) return;
    let payload: EditSubmission;
    try {
      payload = prepareEditSubmission(editSubmission.current, em.id, t, em.edit_version ?? 0);
    } catch (e) {
      setMutErr(e instanceof Error ? e.message : "编辑失败");
      return;
    }
    editSubmission.current = payload;
    mutation.current = token;
    setMutBusy(true);
    setMutErr(null);
    const mine = () =>
      ownsEditResult(
        { token, rev, messageId: em.id, convId: conv.id, gen },
        {
          owner: viewOwner.current,
          rev: draftRevision.current,
          editingId: getState().editingMessage?.id ?? null,
          activeConv: getState().activeConv,
          gen: session(),
        },
      );
    try {
      const updated = await msgsApi.edit(
        payload.id,
        payload.content,
        payload.expectedVersion,
        payload.clientOpId,
      );
      if (!current(token)) return;
      // 服务端确认（含幂等回放视图）按版本合并进列表；
      // 只有用户仍停留在本次编辑态时才退出编辑并还原草稿
      const own = mine();
      setState((st) => ({
        messages: {
          ...st.messages,
          [conv.id]: upsertMessage(st.messages[conv.id] ?? [], updated),
        },
        ...(own ? { editingMessage: null } : {}),
      }));
      if (own) {
        editSubmission.current = null;
        setText(draftBeforeEdit.current);
        draftBeforeEdit.current = "";
      }
    } catch (e) {
      if (!mine()) return;
      if (editRejected(e)) editSubmission.current = null;
      setMutErr(e instanceof Error ? e.message : "编辑失败");
    } finally {
      if (mutation.current === token) mutation.current = null;
      if (current(token)) setMutBusy(false);
    }
  };

  const doDelete = async (m: MessageView) => {
    setMsgMenu(null);
    if (mutBusy) return;
    if (!window.confirm("删除这条消息？所有人将看到“消息已删除”")) return;
    setMutBusy(true);
    setMutErr(null);
    try {
      const d = await msgsApi.remove(m.id, crypto.randomUUID());
      setState((st) => applyUpdatedMessage(st, d));
    } catch (e) {
      setMutErr(e instanceof Error ? e.message : "删除失败");
    } finally {
      setMutBusy(false);
    }
  };

  /** reaction 切换：已表态→取消，未表态→添加；服务端确认后更新 */
  const toggleReaction = async (m: MessageView, emoji: string) => {
    const key = `${m.id}:${emoji}`;
    if (s.reactionPending[key]) return;
    setState((st) => ({
      reactionPending: { ...st.reactionPending, [key]: true },
    }));
    setMutErr(null);
    try {
      const mine = (m.reactions ?? []).some((r) => r.emoji === emoji && r.reacted_by_me);
      const res = await msgsApi.react(m.id, emoji, !mine, crypto.randomUUID());
      setState((st) => applyUpdatedMessage(st, res));
    } catch (e) {
      setMutErr(e instanceof Error ? e.message : "表情操作失败");
    } finally {
      setState((st) => {
        const next = { ...st.reactionPending };
        delete next[key];
        return { reactionPending: next };
      });
    }
  };

  const send = async () => {
    const t = text.trim();
    if (readonly || sending || mutBusy) return;
    if (s.editingMessage) {
      await submitEdit();
      return;
    }
    if (!t && pendingFiles.length === 0) return;
    setSending(true);
    try {
      // 先上传待发附件，文字随最后一个附件消息一起发
      const atts: AttachmentView[] = [];
      for (const f of pendingFiles) {
        atts.push(await files.upload(conv.id, f));
      }
      const rt = s.replyingTo ?? undefined;
      atts.forEach((att, i) =>
        sendChat(conv.id, i === atts.length - 1 ? t : "", att, i === atts.length - 1 ? rt : undefined),
      );
      if (atts.length === 0 && t) sendChat(conv.id, t, undefined, rt);
      setText("");
      setDraft(conv.id, ""); // 发送即清该会话草稿
      setPendingFiles([]);
      setAc(null);
      setState({ replyingTo: null });
      stickBottom.current = true;
    } catch (err) {
      alert(err instanceof Error ? err.message : "发送失败");
    } finally {
      setSending(false);
    }
  };

  const addPending = (list: Iterable<File>) => {
    setPendingFiles((p) => [...p, ...Array.from(list)]);
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
    const next = text.slice(0, ac.start) + "@" + uname + " " + text.slice(caret);
    setText(next);
    if (!s.editingMessage) setDraft(conv.id, next);
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

  // 跳到消息：目标不在本地时拉取以其为中心的窗口（回复预览/搜索结果共用）
  const jumpToRef = async (id: string, seq: number) => {
    setShowSearch(false);
    setFlashId(id);
    if (!(getState().messages[conv.id] ?? []).some((x) => x.id === id)) {
      try {
        const p = await convs.messages(conv.id, {
          after_seq: Math.max(0, seq - 25),
          limit: 50,
        });
        // P1-11：按 id/seq 合并进现有窗口，不能整体替换（会丢弃实时/乐观消息）
        setState((st) => {
          const merged = p.messages.reduce(
            (l, x) => upsertMessage(l, x),
            st.messages[conv.id] ?? [],
          );
          return {
            messages: { ...st.messages, [conv.id]: merged },
            hasMore: { ...st.hasMore, [conv.id]: (merged[0]?.seq ?? 1) > 1 },
          };
        });
      } catch (e) {
        console.warn(e);
      }
    }
    stickBottom.current = false;
    setTimeout(() => {
      scrollRef.current
        ?.querySelector(`[data-mid="${id}"]`)
        ?.scrollIntoView({ block: "center" });
    }, 60);
    setTimeout(() => setFlashId(null), 2600);
  };

  const jump = (m: MessageView) => jumpToRef(m.id, m.seq);

  /** 回复预览点击 → 定位原消息（删除的墓碑仍可定位） */
  const jumpPreview = (p: ReplyPreview) => jumpToRef(p.message_id, p.seq);

  const openPreview = (att: AttachmentView) => {
    setPreview(att);
    setPreviewText(null);
    if (isTextLike(att)) {
      fetch(files.url(att.id))
        .then((r) => r.text())
        .then((t) => setPreviewText(t.slice(0, 300_000)))
        .catch(() => setPreviewText("（加载失败）"));
    }
  };

  const onAttMenu = (e: React.MouseEvent, att: AttachmentView) => {
    e.preventDefault();
    e.stopPropagation(); // 附件菜单优先，不再冒泡到消息菜单
    setMsgMenu(null);
    setCtxMenu({ x: e.clientX, y: e.clientY, att });
  };

  const onFile = (e: React.ChangeEvent<HTMLInputElement>) => {
    const fl = e.target.files;
    e.target.value = "";
    if (!fl?.length || readonly) return;
    addPending(fl);
  };

  // 外面复制的文件/截图直接粘贴进待发区
  const onPaste = (e: React.ClipboardEvent) => {
    const fl = e.clipboardData?.files;
    if (fl && fl.length > 0) {
      e.preventDefault();
      if (!readonly) addPending(fl);
    }
  };

  const onInput = (v: string) => {
    setText(v);
    // V2 P1-3：编辑态下输入框是被编辑消息内容，不能当草稿存
    if (!s.editingMessage) setDraft(conv.id, v);
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
                  <div className="bubble-wrap">
                    {canActMsg(m) && (
                      <div className={`msg-actions ${emojiPick === m.id ? "open" : ""}`}>
                        {emojiPick === m.id ? (
                          <>
                            {QUICK_EMOJI.map((e) => (
                              <button
                                key={e}
                                className="rx-pick"
                                disabled={!!s.reactionPending[`${m.id}:${e}`]}
                                onClick={() => {
                                  setEmojiPick(null);
                                  void toggleReaction(m, e);
                                }}
                              >
                                {e}
                              </button>
                            ))}
                            <button
                              className="rx-pick"
                              title="收起"
                              onClick={() => setEmojiPick(null)}
                            >
                              ✕
                            </button>
                          </>
                        ) : (
                          <>
                            <button
                              title="表情"
                              aria-label="表情"
                              onClick={() => setEmojiPick(m.id)}
                            >
                              😊
                            </button>
                            <button title="回复" aria-label="回复" onClick={() => startReply(m)}>
                              ↩
                            </button>
                            {canEditMsg(m) && (
                              <button
                                title="编辑"
                                aria-label="编辑"
                                onClick={() => startEdit(m)}
                              >
                                ✏️
                              </button>
                            )}
                            {canDeleteMsg(m) && (
                              <button
                                title="删除"
                                aria-label="删除"
                                onClick={() => void doDelete(m)}
                              >
                                🗑
                              </button>
                            )}
                          </>
                        )}
                      </div>
                    )}
                    {m.deleted_at ? (
                      <div className={`bubble deleted ${flashId === m.id ? "flash" : ""}`}>
                        消息已删除
                      </div>
                    ) : (
                      <div
                        className={`bubble ${pending ? "pending" : ""} ${
                          m.mentions.includes(s.me?.id ?? "") ? "mentioned" : ""
                        } ${flashId === m.id ? "flash" : ""}`}
                        onContextMenu={(e) => {
                          if (!canActMsg(m)) return;
                          e.preventDefault();
                          setMsgMenu({ x: e.clientX, y: e.clientY, m });
                        }}
                      >
                        {m.reply_to_preview && (
                          <button
                            type="button"
                            className="reply-quote"
                            onClick={() => jumpPreview(m.reply_to_preview!)}
                          >
                            <span className="rq-name">{m.reply_to_preview.sender_name}</span>
                            <span className="rq-text">
                              {m.reply_to_preview.deleted
                                ? "原消息已删除"
                                : m.reply_to_preview.excerpt ||
                                  kindLabel(m.reply_to_preview.kind)}
                            </span>
                          </button>
                        )}
                        {m.attachment && m.kind === "image" && (
                          <img
                            className="msg-img"
                            src={files.url(m.attachment.id)}
                            alt={m.attachment.file_name}
                            onClick={() => openPreview(m.attachment!)}
                            onContextMenu={(e) => onAttMenu(e, m.attachment!)}
                          />
                        )}
                        {m.attachment && m.kind === "file" && (
                          <div
                            className="msg-file"
                            onClick={() => openPreview(m.attachment!)}
                            onContextMenu={(e) => onAttMenu(e, m.attachment!)}
                          >
                            <span className="file-ic">📄</span>
                            <span className="file-meta">
                              <b>{m.attachment.file_name}</b>
                              <i>{fmtSize(m.attachment.size)}</i>
                            </span>
                          </div>
                        )}
                        {m.kind === "poll" && m.poll ? (
                          <PollCard
                            m={m}
                            conv={conv}
                            members={members}
                            busy={!!pollBusy[m.id]}
                            setBusy={(v) =>
                              setPollBusy((b) => ({ ...b, [m.id]: v }))
                            }
                            onErr={setMutErr}
                          />
                        ) : (
                          showText(m) && <RichText text={m.content} />
                        )}
                        {m.edited_at && <span className="edited-mark">（已编辑）</span>}
                      </div>
                    )}
                    {(m.reactions?.length ?? 0) > 0 && (
                      <div className="reactions">
                        {m.reactions!.map((r) => (
                          <button
                            key={r.emoji}
                            className={`rx-chip ${r.reacted_by_me ? "me" : ""}`}
                            disabled={!!s.reactionPending[`${m.id}:${r.emoji}`]}
                            title={r.reacted_by_me ? "取消表态" : "表态"}
                            onClick={() => void toggleReaction(m, r.emoji)}
                          >
                            {r.emoji}
                            <span className="rx-count">{r.count}</span>
                          </button>
                        ))}
                      </div>
                    )}
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
            <input ref={fileInput} type="file" hidden multiple onChange={onFile} />
            {s.replyingTo && (
              <div className="composer-banner">
                <span className="cb-label">回复 {s.replyingTo.sender_name}</span>
                <span className="cb-text">
                  {s.replyingTo.deleted_at
                    ? "原消息已删除"
                    : s.replyingTo.attachment
                      ? kindLabel(s.replyingTo.kind)
                      : s.replyingTo.content.slice(0, 60)}
                </span>
                <button
                  className="icon-btn"
                  title="取消回复"
                  aria-label="取消回复"
                  onClick={() => setState({ replyingTo: null })}
                >
                  ✕
                </button>
              </div>
            )}
            {s.editingMessage && (
              <div className="composer-banner editing">
                <span className="cb-label">正在编辑消息</span>
                <span className="cb-text">{s.editingMessage.content.slice(0, 60)}</span>
                <button
                  className="icon-btn"
                  title="取消编辑"
                  aria-label="取消编辑"
                  onClick={cancelEdit}
                >
                  ✕
                </button>
              </div>
            )}
            {mutErr && <div className="mut-err">{mutErr}</div>}
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
            {pendingFiles.length > 0 && (
              <div className="pending-files">
                {pendingFiles.map((f, i) => (
                  <span key={i} className="pf-chip" title={f.name}>
                    {f.type.startsWith("image/") ? "🖼" : "📄"}
                    <span className="pf-name">{f.name}</span>
                    <i>{fmtSize(f.size)}</i>
                    <b
                      onClick={() =>
                        setPendingFiles((p) => p.filter((_, j) => j !== i))
                      }
                    >
                      ✕
                    </b>
                  </span>
                ))}
              </div>
            )}
            <div className="composer-row">
              <button
                className="icon-btn attach-btn"
                title="发送图片/文件（也可直接粘贴）"
                onClick={() => fileInput.current?.click()}
              >
                📎
              </button>
              {conv.kind === "group" && (
                <button
                  className="icon-btn"
                  title="发起投票"
                  aria-label="发起投票"
                  onClick={() => setShowPoll(true)}
                >
                  🗳
                </button>
              )}
              <textarea
                ref={taRef}
                value={text}
                placeholder="输入消息，Enter 发送，Shift+Enter 换行"
                onChange={(e) => onInput(e.target.value)}
                onPaste={onPaste}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    if (acList.length > 0) {
                      pickAc(acList[0].user.username);
                    } else {
                      send();
                    }
                  }
                  if (e.key === "Escape") {
                    if (ac) setAc(null);
                    else if (emojiPick) setEmojiPick(null);
                    else if (s.editingMessage) cancelEdit();
                    else if (s.replyingTo) setState({ replyingTo: null });
                  }
                }}
                rows={3}
              />
              <button
                className="primary send-btn"
                onClick={send}
                disabled={
                  sending || mutBusy || (!text.trim() && pendingFiles.length === 0)
                }
              >
                {s.editingMessage ? (mutBusy ? "保存中…" : "保存") : sending ? "发送中…" : "发送"}
              </button>
            </div>
          </>
        )}
      </footer>
      {showInfo && conv.kind === "group" && (
        <GroupInfo conv={conv} onClose={() => setShowInfo(false)} />
      )}
      {showPoll && conv.kind === "group" && (
        <NewPollModal convId={conv.id} onClose={() => setShowPoll(false)} />
      )}

      {/* 附件预览 */}
      {preview && (
        <div className="modal-mask" onClick={() => setPreview(null)}>
          <div className="pv-modal" onClick={(e) => e.stopPropagation()}>
            <div className="pv-head">
              <span className="pv-name" title={preview.file_name}>
                {preview.file_name}
              </span>
              <span className="muted">{fmtSize(preview.size)}</span>
              <button onClick={() => downloadAttachment(preview)}>下载</button>
              <button className="icon-btn" onClick={() => setPreview(null)}>
                ✕
              </button>
            </div>
            <div className="pv-body">
              {preview.mime.startsWith("image/") && (
                <img src={files.url(preview.id)} alt={preview.file_name} />
              )}
              {isTextLike(preview) && !preview.mime.startsWith("image/") && (
                <pre>{previewText ?? "加载中…"}</pre>
              )}
              {preview.mime === "application/pdf" && (
                <iframe src={files.url(preview.id)} title={preview.file_name} />
              )}
              {preview.mime.startsWith("audio/") && (
                <audio controls src={files.url(preview.id)} />
              )}
              {preview.mime.startsWith("video/") && (
                <video controls src={files.url(preview.id)} />
              )}
              {!preview.mime.startsWith("image/") &&
                !preview.mime.startsWith("audio/") &&
                !preview.mime.startsWith("video/") &&
                preview.mime !== "application/pdf" &&
                !isTextLike(preview) && (
                  <div className="pv-unsupported">暂不支持预览此格式，请下载后查看</div>
                )}
            </div>
          </div>
        </div>
      )}

      {/* 消息右键菜单（生命周期操作；可用性与 hover 操作条一致） */}
      {msgMenu && (
        <div
          className="ctx-mask"
          onClick={() => setMsgMenu(null)}
          onContextMenu={(e) => e.preventDefault()}
        >
          <div className="ctx-menu" style={{ left: msgMenu.x, top: msgMenu.y }}>
            <div
              className="ctx-item"
              onClick={() => {
                setEmojiPick(msgMenu.m.id);
                setMsgMenu(null);
              }}
            >
              表情
            </div>
            <div
              className="ctx-item"
              onClick={() => {
                startReply(msgMenu.m);
                setMsgMenu(null);
              }}
            >
              回复
            </div>
            {canEditMsg(msgMenu.m) && (
              <div
                className="ctx-item"
                onClick={() => {
                  startEdit(msgMenu.m);
                  setMsgMenu(null);
                }}
              >
                编辑
              </div>
            )}
            {canDeleteMsg(msgMenu.m) && (
              <div className="ctx-item" onClick={() => void doDelete(msgMenu.m)}>
                删除
              </div>
            )}
          </div>
        </div>
      )}

      {/* 附件右键菜单 */}
      {ctxMenu && (
        <div
          className="ctx-mask"
          onClick={() => setCtxMenu(null)}
          onContextMenu={(e) => e.preventDefault()}
        >
          <div className="ctx-menu" style={{ left: ctxMenu.x, top: ctxMenu.y }}>
            <div
              className="ctx-item"
              onClick={() => {
                openPreview(ctxMenu.att);
                setCtxMenu(null);
              }}
            >
              预览
            </div>
            <div
              className="ctx-item"
              onClick={() => {
                downloadAttachment(ctxMenu.att);
                setCtxMenu(null);
              }}
            >
              下载
            </div>
          </div>
        </div>
      )}
    </main>
  );
}
