import { useEffect, useMemo, useState } from "react";
import { convs } from "../api";
import {
  applyPrefUpdate,
  convAvatarText,
  convTitle,
  isArchived,
  isMuted,
  isPinned,
  setState,
  useApp,
} from "../store";
import type { ConversationView, PrefUpdate } from "../types";
import { openConversation } from "../ws";
import { convPreview } from "./convPreview";
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

/** 静音状态的悬浮说明 */
function muteLabel(c: ConversationView): string {
  const p = c.preference;
  if (!p) return "";
  if (p.mute_forever) return "已永久静音";
  if (p.muted_until && Date.parse(p.muted_until) > Date.now())
    return `静音至 ${new Date(p.muted_until).toLocaleString()}`;
  return "";
}

export default function ConvList() {
  const s = useApp();
  const [q, setQ] = useState("");
  const [tab, setTab] = useState<"chats" | "archived">("chats");
  const [showNewGroup, setShowNewGroup] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number; c: ConversationView } | null>(null);
  const [menuBusy, setMenuBusy] = useState(false);
  const [menuErr, setMenuErr] = useState<string | null>(null);

  const archivedCount = s.conversations.filter(isArchived).length;

  const list = useMemo(() => {
    const inTab = s.conversations.filter((c) =>
      tab === "archived" ? isArchived(c) : !isArchived(c),
    );
    const kw = q.trim().toLowerCase();
    if (!kw) return inTab;
    return inTab.filter((c) => convTitle(c).toLowerCase().includes(kw));
  }, [s.conversations, q, tab]);

  const openMenu = (e: React.MouseEvent, c: ConversationView) => {
    e.preventDefault();
    e.stopPropagation();
    setMenuErr(null);
    // 窄屏收敛：菜单不超出右/下边缘
    setMenu({
      x: Math.min(e.clientX, window.innerWidth - 140),
      y: Math.min(e.clientY, window.innerHeight - 200),
      c,
    });
  };

  /** 偏好提交：服务端确认后合并（不乐观回滚）；同会话一次一个 */
  const submitPref = async (id: string, body: PrefUpdate) => {
    if (menuBusy) return;
    setMenuBusy(true);
    setMenuErr(null);
    try {
      const pref = await convs.setPref(id, body);
      setState((cur) => applyPrefUpdate(cur, id, pref));
      setMenu(null);
    } catch (e) {
      setMenuErr(e instanceof Error ? e.message : "设置失败");
    } finally {
      setMenuBusy(false);
    }
  };

  // Esc 关闭菜单（键盘可操作性）
  useEffect(() => {
    if (!menu) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setMenu(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [menu != null]);

  const muteOneHour = () => new Date(Date.now() + 3600_000).toISOString();
  // 「今天」= 本地下一午夜（转 UTC 提交，§8.3）
  const muteToday = () => {
    const d = new Date();
    d.setHours(24, 0, 0, 0);
    return d.toISOString();
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
      <div className="conv-tabs" role="tablist">
        <button
          role="tab"
          aria-selected={tab === "chats"}
          className={tab === "chats" ? "on" : ""}
          onClick={() => setTab("chats")}
        >
          会话
        </button>
        <button
          role="tab"
          aria-selected={tab === "archived"}
          className={tab === "archived" ? "on" : ""}
          onClick={() => setTab("archived")}
        >
          归档{archivedCount > 0 ? ` (${archivedCount})` : ""}
        </button>
      </div>
      <div className="conv-scroll">
        {list.length === 0 && (
          <div className="empty-hint">
            {tab === "archived" ? (
              "没有归档会话。"
            ) : (
              <>
                还没有会话。
                <br />
                去「联系人」加好友开始聊天吧。
              </>
            )}
          </div>
        )}
        {list.map((c) => {
          const title = convTitle(c, s.me?.id);
          const peerOnline = c.kind === "dm" && c.peer ? !!s.online[c.peer.id] : false;
          const typing = s.typing[c.id];
          const typingFresh = typing && Date.now() - typing.ts < 5000;
          const muted = isMuted(c);
          // V2 P1-3：副标题预览（typing > 草稿 > 最新消息）
          const pv = convPreview(c, {
            draft: s.drafts[c.id],
            active: s.activeConv === c.id,
            typing: !!typingFresh,
            meId: s.me?.id,
          });
          return (
            <div
              key={c.id}
              className={`conv-item ${s.activeConv === c.id ? "on" : ""}`}
              onClick={() => openConversation(c.id)}
              onContextMenu={(e) => openMenu(e, c)}
            >
              <div className={`avatar ${c.kind === "self" ? "self-avatar" : ""}`}>
                {c.kind === "self" ? "🗂" : convAvatarText(c)}
                {peerOnline && <i className="online-dot" />}
              </div>
              <div className="conv-mid">
                <div className="conv-title">
                  {isPinned(c) && <span className="conv-flag" title="已置顶">📌</span>}
                  {title}
                  {muted && (
                    <span className="conv-flag" title={muteLabel(c)}>🔕</span>
                  )}
                </div>
                <div className="conv-sub">
                  {c.has_mention && c.unread_count > 0 && (
                    <span className="mention-tag">[有人@我]</span>
                  )}
                  {pv.kind === "draft" && <span className="draft-tag">[草稿]</span>}
                  {pv.text}
                </div>
              </div>
              <div className="conv-right">
                <div className="conv-time">{fmtTime(c.last_message?.created_at ?? c.created_at)}</div>
                {c.unread_count > 0 && (
                  <div className={`unread ${muted ? "muted" : ""}`}>
                    {c.unread_count > 99 ? "99+" : c.unread_count}
                  </div>
                )}
              </div>
              <button
                className="icon-btn conv-menu-btn"
                title="会话设置"
                aria-label="会话设置"
                onClick={(e) => openMenu(e, c)}
              >
                ⋯
              </button>
            </div>
          );
        })}
      </div>
      {showNewGroup && <NewGroupModal onClose={() => setShowNewGroup(false)} />}

      {/* 会话偏好菜单：置顶 / 静音 / 归档（右键或 ⋯ 按钮打开） */}
      {menu && (
        <div
          className="ctx-mask"
          onClick={() => setMenu(null)}
          onContextMenu={(e) => e.preventDefault()}
        >
          <div className="ctx-menu" style={{ left: menu.x, top: menu.y }}>
            <button
              className="ctx-item"
              disabled={menuBusy}
              onClick={() => void submitPref(menu.c.id, { pinned: !isPinned(menu.c) })}
            >
              {isPinned(menu.c) ? "取消置顶" : "置顶"}
            </button>
            {isMuted(menu.c) ? (
              <button
                className="ctx-item"
                disabled={menuBusy}
                onClick={() => void submitPref(menu.c.id, { mute: { mode: "none" } })}
              >
                取消静音{menu.c.preference?.mute_forever ? "（永久）" : ""}
              </button>
            ) : (
              <>
                <button
                  className="ctx-item"
                  disabled={menuBusy}
                  onClick={() =>
                    void submitPref(menu.c.id, { mute: { mode: "until", until: muteOneHour() } })
                  }
                >
                  静音 1 小时
                </button>
                <button
                  className="ctx-item"
                  disabled={menuBusy}
                  onClick={() =>
                    void submitPref(menu.c.id, { mute: { mode: "until", until: muteToday() } })
                  }
                >
                  静音至明天
                </button>
                <button
                  className="ctx-item"
                  disabled={menuBusy}
                  onClick={() => void submitPref(menu.c.id, { mute: { mode: "forever" } })}
                >
                  永久静音
                </button>
              </>
            )}
            <button
              className="ctx-item"
              disabled={menuBusy}
              onClick={() => void submitPref(menu.c.id, { archived: !isArchived(menu.c) })}
            >
              {isArchived(menu.c) ? "取消归档" : "归档"}
            </button>
            {menuErr && <div className="ctx-err">{menuErr}</div>}
          </div>
        </div>
      )}
    </aside>
  );
}
