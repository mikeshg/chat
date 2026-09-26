import { useEffect, useState } from "react";
import { convs } from "../api";
import { refreshCatalogs } from "../ws";
import { absorbAvatarUsers, setState, useApp } from "../store";
import type { ConversationDetail, ConversationView } from "../types";
import AnnouncementEditor from "./AnnouncementEditor";
import Avatar from "./Avatar";

export default function GroupInfo({
  conv,
  onClose,
}: {
  conv: ConversationView;
  onClose: () => void;
}) {
  const s = useApp();
  const [detail, setDetail] = useState<ConversationDetail | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [newName, setNewName] = useState("");
  const [adding, setAdding] = useState(false);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [editingAnn, setEditingAnn] = useState(false);

  const isOwner = detail?.owner_id === s.me?.id;
  const memberIds = new Set(detail?.members.map((m) => m.user.id));
  const invitable = s.friends.filter((f) => !memberIds.has(f.user.id));

  const reload = async () => {
    const [d, l] = await Promise.all([convs.detail(conv.id), convs.list()]);
    setDetail(d);
    setState((cur) => ({
      conversations: l,
      avatars: absorbAvatarUsers(cur.avatars, [
        ...d.members.map((m) => m.user),
        ...l.map((c) => c.peer),
      ]),
    }));
  };
  useEffect(() => {
    setEditingAnn(false);
    reload().catch(console.warn);
  }, [conv.id]);

  const act = (p: Promise<unknown>) =>
    p.then(reload).catch((e) => alert(e instanceof Error ? e.message : "操作失败"));

  const leave = async () => {
    if (!confirm(isOwner ? "你是群主，退出后将自动转让；确定退出？" : "退出该群？")) return;
    await convs.leave(conv.id);
    await act(Promise.resolve());
    setState({ activeConv: null });
    onClose();
  };

  return (
    <aside className="group-info">
      <div className="gi-head">
        <h3>群聊信息</h3>
        <button className="icon-btn" onClick={onClose}>
          ✕
        </button>
      </div>
      {detail && (
        <>
          <div className="gi-name">
            {renaming ? (
              <div className="row">
                <input value={newName} onChange={(e) => setNewName(e.target.value)} />
                <button
                  className="primary"
                  onClick={() =>
                    act(convs.rename(conv.id, newName.trim())).then(() => setRenaming(false))
                  }
                >
                  保存
                </button>
              </div>
            ) : (
              <>
                <b>{detail.name ?? "群聊"}</b>
                {isOwner && (
                  <button
                    onClick={() => {
                      setNewName(detail.name ?? "");
                      setRenaming(true);
                    }}
                  >
                    改名
                  </button>
                )}
              </>
            )}
          </div>

          <div className="gi-ann">
            <h4>群公告</h4>
            {editingAnn && conv.owner_id === s.me?.id ? (
              <AnnouncementEditor
                initial={conv.announcement?.text ?? ""}
                onCancel={() => setEditingAnn(false)}
                onSave={async (text) => {
                  await convs.setAnnouncement(conv.id, text);
                  await refreshCatalogs();
                  setEditingAnn(false);
                }}
              />
            ) : (
              <>
                <pre className="ann-text">{conv.announcement?.text || "暂无公告"}</pre>
                {conv.announcement?.updated_by_name && (
                  <div className="muted">
                    由 {conv.announcement.updated_by_name} 更新于{" "}
                    {conv.announcement.updated_at
                      ? new Date(conv.announcement.updated_at).toLocaleString()
                      : ""}
                  </div>
                )}
                {conv.owner_id === s.me?.id && (
                  <button type="button" onClick={() => setEditingAnn(true)}>
                    编辑
                  </button>
                )}
              </>
            )}
          </div>

          <div className="gi-members">
            <div className="row between">
              <h4>成员（{detail.members.length}）</h4>
              <button onClick={() => setAdding(!adding)}>拉好友</button>
            </div>
            {adding && (
              <div className="gi-add">
                {invitable.length === 0 && <div className="muted">没有可邀请的好友</div>}
                {invitable.map((f) => (
                  <label className="row pick" key={f.user.id}>
                    <input
                      type="checkbox"
                      checked={picked.has(f.user.id)}
                      onChange={() => {
                        const n = new Set(picked);
                        if (n.has(f.user.id)) n.delete(f.user.id);
                        else n.add(f.user.id);
                        setPicked(n);
                      }}
                    />
                    <span>{f.user.display_name}</span>
                  </label>
                ))}
                {invitable.length > 0 && (
                  <button
                    className="primary"
                    onClick={() =>
                      act(convs.addMembers(conv.id, [...picked])).then(() => {
                        setAdding(false);
                        setPicked(new Set());
                      })
                    }
                  >
                    邀请（{picked.size}）
                  </button>
                )}
              </div>
            )}
            {detail.members.map((m) => (
              <div className="row" key={m.user.id}>
                <Avatar
                  userId={m.user.id}
                  name={m.user.display_name}
                  avatarVersion={m.user.avatar_version}
                  hasAvatar={m.user.has_avatar}
                  size="sm"
                >
                  {s.online[m.user.id] && <i className="online-dot" />}
                </Avatar>
                <div className="grow">
                  {m.user.display_name}
                  {m.role === "owner" && <span className="tag">群主</span>}
                </div>
                {isOwner && m.user.id !== s.me?.id && (
                  <>
                    <button onClick={() => act(convs.transfer(conv.id, m.user.id))}>
                      转让群主
                    </button>
                    <button
                      className="danger"
                      onClick={() => act(convs.kick(conv.id, m.user.id))}
                    >
                      移出
                    </button>
                  </>
                )}
              </div>
            ))}
          </div>

          <div className="gi-actions">
            <button className="danger" onClick={leave}>
              退出群聊
            </button>
            {isOwner && (
              <button
                className="danger"
                onClick={() => {
                  if (confirm("解散后所有成员将被移出，消息记录保留但不可再发言。确定？"))
                    act(convs.dissolve(conv.id)).then(() => {
                      setState({ activeConv: null });
                      onClose();
                    });
                }}
              >
                解散群聊
              </button>
            )}
          </div>
        </>
      )}
    </aside>
  );
}
