import { useState } from "react";
import { auth, friends } from "../api";
import { setState, useApp } from "../store";
import type { FriendView, UserPublic } from "../types";
import { openConversation } from "../ws";
import Avatar from "./Avatar";
import { confirmDialog } from "./Modal";

/** 搜索结果行：已是好友的给「发消息」，非好友走加好友流程 */
export function FoundUserRow({
  u,
  friendList,
  picked,
  msg,
  onPick,
  onMsg,
  onSend,
  onDm,
}: {
  u: UserPublic;
  friendList: FriendView[];
  picked: boolean;
  msg: string;
  onPick: () => void;
  onMsg: (v: string) => void;
  onSend: () => void;
  onDm: () => void;
}) {
  const isFriend = friendList.some((f) => f.user.id === u.id);
  return (
    <div className="row found">
      <Avatar userId={u.id} name={u.display_name} avatarVersion={u.avatar_version} hasAvatar={u.has_avatar} />
      <div className="grow">
        <div>{u.display_name}</div>
        <div className="muted">@{u.username}</div>
      </div>
      {isFriend ? (
        <>
          <span className="muted">已是好友</span>
          <button className="primary" onClick={onDm}>
            发消息
          </button>
        </>
      ) : picked ? (
        <>
          <input
            placeholder="验证消息（可选）"
            value={msg}
            onChange={(e) => onMsg(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && onSend()}
          />
          <button className="primary" onClick={onSend}>
            发送
          </button>
        </>
      ) : (
        <button className="primary" onClick={onPick}>
          加好友
        </button>
      )}
    </div>
  );
}

export default function ContactsPanel() {
  const s = useApp();
  const [q, setQ] = useState("");
  const [results, setResults] = useState<UserPublic[] | null>(null);
  const [picked, setPicked] = useState<UserPublic | null>(null);
  const [msg, setMsg] = useState("");
  const [hint, setHint] = useState("");

  const reload = async () => {
    const [f, r] = await Promise.all([friends.list(), friends.requests()]);
    setState({ friends: f, requests: r });
  };

  const search = async () => {
    setResults(null);
    setPicked(null);
    setHint("");
    if (!q.trim()) return;
    try {
      const r = await auth.searchUser(q.trim());
      setResults(r);
      if (r.length === 0) setHint("没有匹配的用户");
    } catch {
      setHint("搜索失败");
    }
  };

  const sendReq = async () => {
    if (!picked) return;
    try {
      await friends.sendRequest(picked.username, msg);
      setHint("已发送好友请求");
      setPicked(null);
      setResults(null);
      setQ("");
      setMsg("");
      void reload();
    } catch (e) {
      setHint(e instanceof Error ? e.message : "发送失败");
    }
  };

  const openDm = (u: UserPublic) => {
    const c = s.conversations.find((c) => c.kind === "dm" && c.peer?.id === u.id);
    // 走统一入口：已读上报 + 归档会话打开即解除归档（§8.3）
    if (c) openConversation(c.id);
    else setState({ tab: "chats", activeConv: null });
  };

  const removeFriend = async (u: UserPublic) => {
    if (
      !(await confirmDialog(`删除好友 ${u.display_name}？会话将变为只读。`, {
        danger: true,
        okText: "删除",
      }))
    )
      return;
    await friends.remove(u.id);
    await reload();
  };

  return (
    <main className="contacts">
      <h2>联系人</h2>

      <section className="card">
        <h3>添加好友</h3>
        <div className="row">
          <input
            placeholder="搜索用户名或昵称"
            value={q}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && search()}
          />
          <button onClick={search}>搜索</button>
        </div>
        {results?.map((u) => (
          <FoundUserRow
            key={u.id}
            u={u}
            friendList={s.friends}
            picked={picked?.id === u.id}
            msg={msg}
            onPick={() => setPicked(u)}
            onMsg={setMsg}
            onSend={sendReq}
            onDm={() => openDm(u)}
          />
        ))}
        {hint && <div className="muted">{hint}</div>}
      </section>

      <section className="card">
        <h3>新的朋友 {s.requests.incoming.length > 0 && `(${s.requests.incoming.length})`}</h3>
        {s.requests.incoming.length === 0 && s.requests.outgoing.length === 0 && (
          <div className="muted">暂无好友请求</div>
        )}
        {s.requests.incoming.map((r) => (
          <div className="row" key={r.id}>
            <Avatar userId={r.user.id} name={r.user.display_name} avatarVersion={r.user.avatar_version} hasAvatar={r.user.has_avatar} />
            <div className="grow">
              <div>{r.user.display_name}</div>
              <div className="muted">{r.message || "请求加你为好友"}</div>
            </div>
            <button
              className="primary"
              onClick={() => friends.accept(r.id).then(reload)}
            >
              接受
            </button>
            <button onClick={() => friends.decline(r.id).then(reload)}>拒绝</button>
          </div>
        ))}
        {s.requests.outgoing.map((r) => (
          <div className="row" key={r.id}>
            <Avatar userId={r.user.id} name={r.user.display_name} avatarVersion={r.user.avatar_version} hasAvatar={r.user.has_avatar} />
            <div className="grow">
              <div>{r.user.display_name}</div>
              <div className="muted">等待对方通过</div>
            </div>
          </div>
        ))}
      </section>

      <section className="card">
        <h3>我的好友（{s.friends.length}）</h3>
        {s.friends.map((f) => (
          <div className="row" key={f.user.id}>
            <Avatar userId={f.user.id} name={f.user.display_name} avatarVersion={f.user.avatar_version} hasAvatar={f.user.has_avatar}>
              {s.online[f.user.id] && <i className="online-dot" />}
            </Avatar>
            <div className="grow">
              <div>{f.user.display_name}</div>
              <div className="muted">
                @{f.user.username} · {f.user.is_bot ? "机器人" : s.online[f.user.id] ? "在线" : "离线"}
              </div>
            </div>
            <button className="primary" onClick={() => openDm(f.user)}>
              发消息
            </button>
            <button className="danger" onClick={() => removeFriend(f.user)}>
              删除
            </button>
          </div>
        ))}
      </section>
    </main>
  );
}
