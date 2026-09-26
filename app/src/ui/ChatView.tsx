import { useEffect, useRef, useState } from "react";
import { canUseAgent } from "../agentAccess";
import { agents as agentsApi, ApiError, convs, files, msgs as msgsApi, serverInfo, session } from "../api";
import {
  applyInvocations,
  applyLiveMessage,
  applyRefreshed,
  applyUpdatedMessage,
  convTitle,
  getState,
  fetchForward,
  isOwnMessage,
  prependMessages,
  pushToast,
  setDraft,
  setState,
  upsertMessage,
  useApp,
} from "../store";
import { unsentText } from "../drafts";
import type { AgentView, AttachmentView, MemberView, MessageView, ReplyPreview } from "../types";
import { resolveMentions, retainExplicit, type ExplicitTarget } from "../mentionResolve";
import { sendChat, sendTyping } from "../ws";
import RichText from "../richText";
import AnnouncementBar from "./AnnouncementBar";
import AudioBubble from "./AudioBubble";
import ConvMedia from "./ConvMedia";
import ForwardModal from "./ForwardModal";
import GroupInfo from "./GroupInfo";
import { meetingMessage } from "./meeting";
import PinnedBar from "./PinnedBar";
import NewPollModal from "./NewPollModal";
import PollCard from "./PollCard";
import { imageBubbleSize } from "./mediaFormat";
import AgentAvatar from "./AgentAvatar";
import Avatar from "./Avatar";
import VoiceRecorder from "./VoiceRecorder";
import InvocationStatus from "./InvocationStatus";

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/** 附件消息的 content 是服务端占位文本时不再重复渲染 */
function showText(m: MessageView): boolean {
  if (!m.attachment) return true;
  return (
    m.content !== "[图片]" &&
    m.content !== "[语音]" &&
    m.content !== `[文件] ${m.attachment.file_name}`
  );
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
  if (kind === "audio") return "[语音]";
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
  el.href = files.url(a.id, { dl: true });
  el.download = a.file_name;
  document.body.appendChild(el);
  el.click();
  el.remove();
}

/** 图片气泡占位尺寸（V2 P2-1）：按原始宽高比预留，加载前不跳版；无尺寸回退 CSS 上限 */
function imgDim(att: AttachmentView): React.CSSProperties | undefined {
  if (att.width == null || att.height == null) return undefined;
  const d = imageBubbleSize(att.width, att.height);
  if (d.width === 0) return undefined;
  return { width: d.width, height: d.height };
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
  // A11：私聊/群补全用的本人 Agent；显式选中随正文里的 @名字 存在
  const [myAgents, setMyAgents] = useState<AgentView[]>([]);
  const [agentPicks, setAgentPicks] = useState<ExplicitTarget[]>([]);
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
  // 发送（含上传附件）失败的服务端文案，显示在附件条处（V2 P2-1：413 配额提示走这里）
  const [sendErr, setSendErr] = useState<string | null>(null);
  const [mutBusy, setMutBusy] = useState(false);
  // 投票：创建弹窗开关 + 每条消息的操作 busy 态
  const [showPoll, setShowPoll] = useState(false);
  const [pollBusy, setPollBusy] = useState<Record<string, boolean>>({});
  // V2 P2-4：转发弹窗的源消息（收藏不走弹窗，直接调接口到 self 会话）
  const [fwdMsg, setFwdMsg] = useState<MessageView | null>(null);
  const [pinReload, setPinReload] = useState(0);
  const [pinnedIds, setPinnedIds] = useState<Set<string>>(new Set());
  const [meetingBase, setMeetingBase] = useState("");
  // A02：开新话题后的本地分隔（不落库）。convId 对不上就不当成当前会话的标记。
  const [topicState, setTopicState] = useState<{
    convId: string;
    marks: { n: number; afterId: string | null }[];
  }>({ convId: "", marks: [] });
  const [topicBusy, setTopicBusy] = useState(false);
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
  const topicMarks = conv && topicState.convId === conv.id ? topicState.marks : [];
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
    setAgentPicks([]);
    setShowSearch(false);
    setSearchRes(null);
    setFlashId(null);
    setEmojiPick(null);
    setMsgMenu(null);
    setMutErr(null);
    // 待发附件属于旧会话，不能带到新会话里发出去
    setPendingFiles([]);
    setSendErr(null);
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

  // 打开会话，以及收到 pins_updated 时，刷新置顶列表与菜单状态
  useEffect(() => {
    if (!conv) return;
    let alive = true;
    setPinnedIds(new Set());
    const load = () => {
      convs
        .pins(conv.id)
        .then((rows) => {
          if (alive) setPinnedIds(new Set(rows.map((p) => p.message.id)));
        })
        .catch(() => {
          if (alive) setPinnedIds(new Set());
        });
      setPinReload((n) => n + 1);
    };
    load();
    const onPins = (e: Event) => {
      if ((e as CustomEvent<string>).detail === conv.id) load();
    };
    window.addEventListener("chat:pins", onPins);
    return () => {
      alive = false;
      window.removeEventListener("chat:pins", onPins);
    };
  }, [conv?.id]);

  useEffect(() => {
    let alive = true;
    serverInfo()
      .then((info) => {
        if (alive) setMeetingBase(info.meeting_base_url ?? "");
      })
      .catch(() => {
        if (alive) setMeetingBase("");
      });
    return () => {
      alive = false;
    };
  }, []);

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

  // A11：任意会话打开时拉一次调用历史（实时推进仍靠 agent_invocation 帧）
  useEffect(() => {
    if (!conv) return;
    let alive = true;
    agentsApi
      .invocations(conv.id)
      .then((rows) => {
        if (alive) setState((st) => applyInvocations(st, rows));
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [conv?.id]);

  // 私聊和群的 @ 补全要带上本人未删除的 Agent
  useEffect(() => {
    if (!conv || (conv.kind !== "group" && conv.kind !== "dm")) {
      setMyAgents([]);
      return;
    }
    let alive = true;
    const load = () => {
      agentsApi
        .list()
        .then((rows) => {
          if (alive) setMyAgents(rows.filter((a) => !a.deleted));
        })
        .catch(() => {
          if (alive) setMyAgents([]);
        });
    };
    load();
    window.addEventListener("chat:agents", load);
    return () => {
      alive = false;
      window.removeEventListener("chat:agents", load);
    };
  }, [conv?.id, conv?.kind]);

  useEffect(() => {
    setTopicBusy(false);
  }, [conv?.id]);

  // 新消息滚到底（若本来就在底部）
  useEffect(() => {
    if (stickBottom.current && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [msgs.length, conv?.id, topicMarks.length]);

  // V2 P3-1：消费全局搜索跳转 —— 会话打开后定位并闪烁目标消息，随后清空
  useEffect(() => {
    const pj = s.pendingJump;
    if (!pj || !conv || pj.convId !== conv.id) return;
    setState({ pendingJump: null });
    void jumpToRef(pj.messageId, pj.seq);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [s.pendingJump, conv?.id]);

  if (!conv) {
    return (
      <main className="chat-view empty">
        <div className="empty-hint">选择一个会话开始聊天</div>
      </main>
    );
  }

  const title = convTitle(conv, s.me?.id);
  const peerOnline = conv.kind === "dm" && conv.peer ? !!s.online[conv.peer.id] : false;
  // A01：agent 会话副标题 = 宿主名 + 在线态；消息 id → 调用记录（状态行）
  const agentHost = conv.kind === "agent" && conv.agent
    ? s.agentHosts.find((h) => h.id === conv.agent!.host_id)
    : undefined;
  const invs = s.invocations[conv.id] ?? {};
  // A04：409 = 调用已经结束，静默拉一次历史把标签对齐
  const cancelInvocation = (id: string) => {
    void (async () => {
      try {
        const row = await agentsApi.cancel(id);
        setState((st) => applyInvocations(st, [row]));
      } catch (e) {
        if (!(e instanceof ApiError) || e.status !== 409) return;
        try {
          const rows = await agentsApi.invocations(conv.id);
          setState((st) => applyInvocations(st, rows));
        } catch {
          /* 刷新失败也保持安静 */
        }
      }
    })();
  };
  const typing = s.typing[conv.id];
  const typingFresh = typing && Date.now() - typing.ts < 5000;
  const readonly = conv.kind === "dm" && conv.peer && !s.friends.some((f) => f.user.id === conv.peer!.id);
  const agentDeleted = conv.kind === "agent" && !!conv.agent?.deleted;
  const agentClosed = conv.kind === "agent" && !canUseAgent(s.me);
  const composerLocked = agentDeleted || agentClosed;

  // 生命周期入口可见性（服务端在锁内二次校验；这里只做 UI 层收敛）
  // A01：Agent 消息 sender_id 是主人但归属 Agent —— 不可编辑；删除仍允许（主人的会话）
  const canEditMsg = (m: MessageView) =>
    !m.id.startsWith("client:") &&
    !m.deleted_at &&
    isOwnMessage(m, s.me?.id) &&
    m.kind === "text" &&
    withinEditWindow(m);
  const canDeleteMsg = (m: MessageView) =>
    !m.id.startsWith("client:") &&
    !m.deleted_at &&
    (m.sender_id === s.me?.id || (conv.kind === "group" && conv.owner_id === s.me?.id));
  const canActMsg = (m: MessageView) =>
    !m.id.startsWith("client:") && !m.deleted_at && !readonly;
  // V2 P2-4 转发/收藏入口：非乐观消息、非墓碑、非投票。只读会话仍允许（
  // 成员读得到就能转出去；目标侧权限由服务端逐条判定）
  const canForwardMsg = (m: MessageView) =>
    !m.id.startsWith("client:") && !m.deleted_at && m.kind !== "poll";
  // 投票可置顶；已删除、乐观消息、只读会话不显示入口
  const canPinMsg = (m: MessageView) => canActMsg(m);

  const doPin = async (m: MessageView) => {
    setMsgMenu(null);
    setMutErr(null);
    const pinned = pinnedIds.has(m.id);
    try {
      if (pinned) await convs.unpin(conv.id, m.id);
      else await convs.pin(conv.id, m.id);
      setPinnedIds((prev) => {
        const next = new Set(prev);
        if (pinned) next.delete(m.id);
        else next.add(m.id);
        return next;
      });
      setPinReload((n) => n + 1);
    } catch (e) {
      setMutErr(e instanceof Error ? e.message : "置顶失败");
    }
  };

  const startMeeting = () => {
    if (!meetingBase || readonly) return;
    if (!confirm("发起视频会议？将在本会话发送一条会议链接。")) return;
    sendChat(conv.id, meetingMessage(meetingBase));
  };

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
    setAgentPicks([]);
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

  /** 收藏（V2 P2-4）= 转发到本人文件传输助手；每次点击是新 op（重发即有意重复收藏）。
   * 服务端确认后立即并入 self 会话本地缓存；广播帧到达按 id 去重。 */
  const doFavorite = async (m: MessageView) => {
    setMsgMenu(null);
    const self = s.conversations.find((c) => c.kind === "self");
    if (!self || mutBusy) return;
    setMutBusy(true);
    setMutErr(null);
    try {
      const resp = await msgsApi.forward(m.id, [self.id], crypto.randomUUID());
      const r = resp.results[0];
      if (r?.ok && r.message) {
        setState((st) => ({ ...st, ...applyLiveMessage(st, r.message!) }));
        pushToast({ convId: self.id, title: "收藏", text: "已收藏到文件传输助手" });
      } else {
        setMutErr(r?.message_text ?? "收藏失败");
      }
    } catch (e) {
      setMutErr(e instanceof Error ? e.message : "收藏失败");
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
    if (conv?.kind === "agent" && (conv.agent?.deleted || !canUseAgent(s.me))) return;
    if (s.editingMessage) {
      await submitEdit();
      return;
    }
    if (!t && pendingFiles.length === 0) return;
    setSending(true);
    setSendErr(null);
    // 上传期间用户可能继续输入、加附件、切会话：完成后只清本次发出的那部分
    const sentText = text;
    const sentFiles = pendingFiles;
    const rt0 = s.replyingTo;
    const still = () => getState().activeConv === conv.id;
    try {
      // 先上传待发附件，文字随最后一个附件消息一起发
      const atts: AttachmentView[] = [];
      for (const f of sentFiles) {
        atts.push(await files.upload(conv.id, f));
      }
      const rt = rt0 ?? undefined;
      const targets = resolveMentions({
        text: t,
        explicit: agentPicks,
        myAgents: myAgents.map((a) => ({ id: a.id, name: a.name })),
        members: members
          .filter((m) => m.user.id !== s.me?.id)
          .map((m) => ({ id: m.user.id, username: m.user.username })),
      }).agentTargets;
      atts.forEach((att, i) =>
        sendChat(
          conv.id,
          i === atts.length - 1 ? t : "",
          att,
          i === atts.length - 1 ? rt : undefined,
          i === atts.length - 1 ? targets : undefined,
        ),
      );
      if (atts.length === 0 && t) sendChat(conv.id, t, undefined, rt, targets);
      const draft = getState().drafts[conv.id] ?? "";
      if (unsentText(draft, sentText) !== draft) setDraft(conv.id, unsentText(draft, sentText)); // 发送即清该会话草稿
      if (getState().replyingTo === rt0) setState({ replyingTo: null });
      if (still()) {
        setText((cur) => unsentText(cur, sentText));
        setAgentPicks([]);
        setPendingFiles((p) => p.filter((f) => !sentFiles.includes(f)));
        setAc(null);
        stickBottom.current = true;
      }
    } catch (err) {
      // 保留服务端文案（如 413 配额提示）；待发附件不清，便于重试
      if (still()) setSendErr(err instanceof Error ? err.message : "发送失败");
    } finally {
      setSending(false);
    }
  };

  const addPending = (list: Iterable<File>) => {
    setPendingFiles((p) => [...p, ...Array.from(list)]);
  };

  type AcItem =
    | { kind: "agent"; id: string; name: string; online: boolean }
    | { kind: "member"; id: string; username: string; displayName: string };
  // @补全：私聊只有本人 Agent；群里 Agent 排在成员前面（同名时先点到 Agent）
  const acItems: AcItem[] = ac
    ? [
        ...(canUseAgent(s.me) ? myAgents : [])
          .filter((a) => a.name.toLowerCase().startsWith(ac.prefix.toLowerCase()))
          .map((a) => ({
            kind: "agent" as const,
            id: a.id,
            name: a.name,
            online: s.agentHosts.find((h) => h.id === a.host_id)?.online ?? a.host_online,
          })),
        ...(conv.kind === "group"
          ? members
              .filter(
                (x) =>
                  x.user.id !== s.me?.id &&
                  (x.user.username.toLowerCase().startsWith(ac.prefix.toLowerCase()) ||
                    x.user.display_name.toLowerCase().includes(ac.prefix.toLowerCase())),
              )
              .map((x) => ({
                kind: "member" as const,
                id: x.user.id,
                username: x.user.username,
                displayName: x.user.display_name,
              }))
          : []),
      ].slice(0, 8)
    : [];

  const pickAc = (item: AcItem) => {
    if (!ac) return;
    const token = item.kind === "agent" ? item.name : item.username;
    const caret = taRef.current?.selectionStart ?? text.length;
    const next = text.slice(0, ac.start) + "@" + token + " " + text.slice(caret);
    setText(next);
    if (!s.editingMessage) {
      setDraft(conv.id, next);
      const pick: ExplicitTarget =
        item.kind === "agent"
          ? { kind: "agent", id: item.id, token }
          : { kind: "member", id: item.id, token };
      setAgentPicks((prev) => retainExplicit(next, [...prev, pick]));
    }
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
  const jumpToRef = (id: string, seq: number) => {
    const token = ++jumpRequest.current;
    void jumpToMessage(id, seq, token);
  };

  const jumpToMessage = async (id: string, seq: number, token: number) => {
    setShowSearch(false);
    setFlashId(id);
    if (!(getState().messages[conv.id] ?? []).some((x) => x.id === id)) {
      try {
        // 补到缓存最早一条为止，不留断档；缓存为空时补到最新（随后的首屏历史与之重叠）
        const cached = (getState().messages[conv.id] ?? []).filter((x) => !x.id.startsWith("client:"));
        const stop = cached[0]?.seq ?? conv.last_seq + 1;
        const got = await fetchForward(
          (after) => convs.messages(conv.id, { after_seq: after, limit: 200 }),
          Math.max(0, seq - 25),
          stop,
        );
        if (token !== jumpRequest.current) return;
        // P1-11：按 id/seq 合并进现有窗口，不能整体替换（会丢弃实时/乐观消息）
        setState((st) => {
          const merged = got.reduce(
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
    const scrollTo = () => {
      if (token !== jumpRequest.current) return;
      scrollRef.current
        ?.querySelector(`[data-mid="${id}"]`)
        ?.scrollIntoView({ block: "center" });
    };
    setTimeout(scrollTo, 60);
    setTimeout(scrollTo, 300);
    setTimeout(() => {
      if (token === jumpRequest.current) setFlashId(null);
    }, 2600);
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
    // files 是活的 FileList：必须先拷成数组再清 value，否则清空后列表跟着归零
    const picked = Array.from(e.target.files ?? []);
    e.target.value = "";
    if (!picked.length || readonly) return;
    addPending(picked);
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
    setAgentPicks((prev) => {
      const next = retainExplicit(v, prev);
      if (
        next.length === prev.length &&
        next.every((p, i) => p.kind === prev[i].kind && p.id === prev[i].id && p.token === prev[i].token)
      ) {
        return prev;
      }
      return next;
    });
    // @补全：光标前最近的 @ 前缀。群里含成员；私聊只有 Agent
    const caret = taRef.current?.selectionStart ?? v.length;
    const mm = /(^|[\s\n])@([A-Za-z0-9_-]*)$/.exec(v.slice(0, caret));
    const mentionable = conv.kind === "group" || conv.kind === "dm";
    setAc(mentionable && mm ? { start: caret - mm[2].length - 1, prefix: mm[2] } : null);
    const now = Date.now();
    // self/agent 会话不广播 typing（服务端同样短路兜底）
    if (conv.kind !== "self" && conv.kind !== "agent" && now - lastTypingSent.current > 2500) {
      lastTypingSent.current = now;
      sendTyping(conv.id);
    }
  };

  const startNewTopic = async () => {
    if (topicBusy) return;
    setTopicBusy(true);
    try {
      await agentsApi.resetContext(conv.id);
      const afterId = msgs.length ? msgs[msgs.length - 1].id : null;
      setTopicState((st) => {
        const marks = st.convId === conv.id ? st.marks : [];
        return { convId: conv.id, marks: [...marks, { n: marks.length + 1, afterId }] };
      });
      pushToast({ convId: conv.id, title: "已开始新话题", text: "" });
    } catch (e) {
      pushToast({
        convId: conv.id,
        title: "开新话题失败",
        text: e instanceof ApiError ? e.message : "请稍后重试",
      });
    } finally {
      setTopicBusy(false);
    }
  };

  const topicBreak = (n: number) => (
    <div key={`topic-${n}`} className="msg-sys" data-topic-break="">
      —— 新话题 ——
    </div>
  );

  return (
    <main className="chat-view">
      <header className="chat-head">
        {conv.kind === "agent" && conv.agent && (
          <AgentAvatar
            agentId={conv.agent.id}
            name={conv.agent.name}
            version={conv.agent.avatar_version}
          />
        )}
        <div>
          <div className="chat-title">{title}</div>
          <div className="chat-status">
            {conv.kind === "self"
              ? "在你的设备间同步消息与文件"
              : conv.kind === "agent"
                ? `${agentHost ? (agentHost.label || "宿主") + (agentHost.platform ? ` (${agentHost.platform})` : "") : "宿主"} · ${agentDeleted ? "已删除" : conv.agent?.host_online ? "在线" : "离线"}`
                : conv.kind === "dm"
                  ? peerOnline
                    ? "在线"
                    : conv.peer?.last_seen_at
                      ? `最后在线 ${new Date(conv.peer.last_seen_at).toLocaleString()}`
                      : "离线"
                  : `${conv.member_count} 名成员`}
          </div>
        </div>
        {conv.kind === "agent" && (
          <button
            className="icon-btn chat-head-btn topic-btn"
            type="button"
            title="开新话题"
            disabled={topicBusy || agentClosed}
            onClick={() => void startNewTopic()}
          >
            开新话题
          </button>
        )}
        {meetingBase && conv.kind !== "self" && conv.kind !== "agent" && !readonly && (
          <button className="icon-btn chat-head-btn" title="发起会议" onClick={startMeeting}>
            📹 发起会议
          </button>
        )}
        <button
          className="icon-btn chat-head-btn"
          title="搜索聊天记录"
          onClick={() => {
            setShowSearch(!showSearch);
            setSearchQ("");
            setSearchRes(null);
          }}
        >
          🔍
        </button>
        {/* V2 P3-1：群=群信息（含文件与图片页签）；私聊/文件传输助手=文件与图片入口；A01：agent 会话无群信息入口 */}
        {conv.kind !== "agent" && (
          <button
            className="icon-btn chat-head-btn"
            title={conv.kind === "group" ? "群信息" : "文件与图片"}
            onClick={() => setShowInfo(!showInfo)}
          >
            ⋯
          </button>
        )}
      </header>

      {conv.kind === "group" && (
        <AnnouncementBar
          key={conv.id}
          convId={conv.id}
          announcement={conv.announcement}
          canEdit={conv.owner_id === s.me?.id}
        />
      )}
      <PinnedBar
        convId={conv.id}
        readonly={!!readonly}
        reloadKey={pinReload}
        onJump={jumpToRef}
      />

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
        {topicMarks.filter((t) => t.afterId == null).map((t) => topicBreak(t.n))}
        {msgs.map((m, i) => {
          // A01：agent 发的消息 sender_id 也是主人 —— 归属判定统一走 isOwnMessage
          const mine = isOwnMessage(m, s.me?.id);
          const pending = m.id.startsWith("client:");
          const rowInvs = invs[m.id] ?? [];
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
                {!mine &&
                  (m.agent ? (
                    <AgentAvatar
                      agentId={m.agent.id}
                      name={m.agent.name}
                      version={m.agent.avatar_version}
                      size="sm"
                    />
                  ) : (
                    <Avatar
                      userId={m.sender_id}
                      name={m.sender_name}
                      avatarVersion={m.sender_avatar_version}
                      hasAvatar={m.sender_has_avatar}
                      size="sm"
                    />
                  ))}
                <div
                  className="msg-body"
                  onContextMenu={(e) => {
                    if (m.deleted_at || (!canActMsg(m) && !canForwardMsg(m) && !canPinMsg(m))) return;
                    e.preventDefault();
                    setMsgMenu({ x: e.clientX, y: e.clientY, m });
                  }}
                >
                  {!mine && m.agent ? (
                    <div className="msg-sender">
                      {m.agent.name}
                      <span className="agent-tag">Agent</span>
                    </div>
                  ) : (
                    !mine &&
                    conv.kind === "group" && (
                      <div className="msg-sender">{m.sender_name}</div>
                    )
                  )}
                  <div className="bubble-wrap">
                    {(canActMsg(m) || canForwardMsg(m) || canPinMsg(m)) && (
                      <div className={`msg-actions ${emojiPick === m.id ? "open" : ""}`}>
                        {emojiPick === m.id && canActMsg(m) ? (
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
                            {canActMsg(m) && (
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
                            {canPinMsg(m) && (
                              <button
                                title={pinnedIds.has(m.id) ? "取消置顶" : "置顶"}
                                aria-label={pinnedIds.has(m.id) ? "取消置顶" : "置顶"}
                                onClick={() => void doPin(m)}
                              >
                                📌
                              </button>
                            )}
                            {canForwardMsg(m) && (
                              <>
                                <button
                                  title="转发"
                                  aria-label="转发"
                                  onClick={() => setFwdMsg(m)}
                                >
                                  ➦
                                </button>
                                <button
                                  title="收藏"
                                  aria-label="收藏"
                                  onClick={() => void doFavorite(m)}
                                >
                                  ⭐
                                </button>
                              </>
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
                          if (!canActMsg(m) && !canForwardMsg(m) && !canPinMsg(m)) return;
                          e.preventDefault();
                          setMsgMenu({ x: e.clientX, y: e.clientY, m });
                        }}
                      >
                        {m.forwarded_from && (
                          <div className="fwd-from">转发自 {m.forwarded_from}</div>
                        )}
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
                            src={files.url(m.attachment.id, {
                              thumb: !!m.attachment.has_thumb,
                            })}
                            alt={m.attachment.file_name}
                            style={imgDim(m.attachment)}
                            onClick={() => openPreview(m.attachment!)}
                            onContextMenu={(e) => onAttMenu(e, m.attachment!)}
                          />
                        )}
                        {m.attachment && m.kind === "audio" && (
                          <AudioBubble att={m.attachment} />
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
              {/* A11：触发消息下的调用状态。群成员看得到，取消只给该 Agent 的主人 */}
              {rowInvs.map((inv) => (
                <InvocationStatus
                  key={inv.id}
                  inv={inv}
                  allowCancel={
                    conv.kind === "agent" || myAgents.some((a) => a.id === inv.agent_id)
                  }
                  onCancel={cancelInvocation}
                />
              ))}
              {topicMarks.filter((t) => t.afterId === m.id).map((t) => topicBreak(t.n))}
            </div>
          );
        })}
        {topicMarks
          .filter((t) => t.afterId != null && !msgs.some((m) => m.id === t.afterId))
          .map((t) => topicBreak(t.n))}
        {typingFresh && <div className="msg-sys">{typing.user.display_name} 正在输入…</div>}
      </div>

      <footer className="composer">
        {readonly ? (
          <div className="readonly-hint">对方已不是你的好友，会话只读</div>
        ) : (
          <>
            {agentClosed && (
              <div className="readonly-hint">管理员已关闭你的 Agent 功能</div>
            )}
            {agentDeleted && !agentClosed && <div className="readonly-hint">该 Agent 已删除</div>}
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
            {acItems.length > 0 && (
              <div className="ac-list">
                {acItems.map((item) => (
                  <div
                    key={item.kind + item.id}
                    className={`ac-item ${item.kind === "agent" ? "ac-agent" : ""}`}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      pickAc(item);
                    }}
                  >
                    {item.kind === "agent" ? (
                      <>
                        <span className="ac-mark">🤖</span>
                        <span className="ac-name">@{item.name}</span>
                        <span className={item.online ? "ac-online" : "ac-offline"}>
                          {item.online ? "在线" : "离线"}
                        </span>
                      </>
                    ) : (
                      <>
                        <span className="ac-name">@{item.username}</span>
                        <span className="ac-disp">{item.displayName}</span>
                      </>
                    )}
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
            {sendErr && <div className="pf-err">{sendErr}</div>}
            <div className="composer-row">
              <button
                className="icon-btn attach-btn"
                title="发送图片/文件（也可直接粘贴）"
                disabled={composerLocked}
                onClick={() => fileInput.current?.click()}
              >
                📎
              </button>
              <VoiceRecorder
                convId={conv.id}
                disabled={sending || mutBusy || composerLocked}
                onError={setMutErr}
              />
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
                disabled={composerLocked}
                placeholder={
                  agentClosed
                    ? "管理员已关闭你的 Agent 功能"
                    : agentDeleted
                      ? "该 Agent 已删除"
                      : "输入消息，Enter 发送，Shift+Enter 换行"
                }
                onChange={(e) => onInput(e.target.value)}
                onPaste={onPaste}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    if (acItems.length > 0) {
                      pickAc(acItems[0]);
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
                  composerLocked || sending || mutBusy || (!text.trim() && pendingFiles.length === 0)
                }
              >
                {s.editingMessage ? (mutBusy ? "保存中…" : "保存") : sending ? "发送中…" : "发送"}
              </button>
            </div>
          </>
        )}
      </footer>
      {showInfo && conv.kind === "group" && (
        <GroupInfo
          conv={conv}
          onClose={() => setShowInfo(false)}
          onPreview={openPreview}
        />
      )}
      {showInfo && conv.kind !== "group" && conv.kind !== "agent" && (
        <aside className="group-info">
          <div className="gi-head">
            <h3>{conv.kind === "self" ? "文件传输助手" : "文件与图片"}</h3>
            <button className="icon-btn" onClick={() => setShowInfo(false)}>
              ✕
            </button>
          </div>
          <ConvMedia conv={conv} onPreview={openPreview} />
        </aside>
      )}
      {showPoll && conv.kind === "group" && (
        <NewPollModal convId={conv.id} onClose={() => setShowPoll(false)} />
      )}
      {fwdMsg && (
        <ForwardModal message={fwdMsg} onClose={() => setFwdMsg(null)} />
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
            {canActMsg(msgMenu.m) && (
              <>
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
              </>
            )}
            {canPinMsg(msgMenu.m) && (
              <div className="ctx-item" onClick={() => void doPin(msgMenu.m)}>
                {pinnedIds.has(msgMenu.m.id) ? "取消置顶" : "置顶"}
              </div>
            )}
            {canForwardMsg(msgMenu.m) && (
              <>
                <div
                  className="ctx-item"
                  onClick={() => {
                    setFwdMsg(msgMenu.m);
                    setMsgMenu(null);
                  }}
                >
                  转发
                </div>
                <div className="ctx-item" onClick={() => void doFavorite(msgMenu.m)}>
                  收藏
                </div>
              </>
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
