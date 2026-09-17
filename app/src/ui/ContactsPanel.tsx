import { useState } from "react";
import { ApiError, auth, friends } from "../api";
import { setState, useApp } from "../store";
import type { UserPublic } from "../types";

export default function ContactsPanel() {
  const s = useApp();
  const [q, setQ] = useState("");
  const [found, setFound] = useState<UserPublic | null>(null);
  const [msg, setMsg] = useState("");
  const [hint, setHint] = useState("");

  const reload = async () => {
    const [f, r] = await Promise.all([friends.list(), friends.requests()]);
    setState({ friends: f, requests: r });
  };

  const search = async () => {
    setFound(null);
    setHint("");
    if (!q.trim()) return;
    try {
      setFound(await auth.searchUser(q.trim()));
    } catch (e) {
      setHint(e instanceof ApiError && e.status === 404 ? "没有这个用户" : "搜索失败");
    }
  };

  const sendReq = async () => {
    if (!found) return;
    try {
      await friends.sendRequest(found.username, msg);
      setHint("已发送好友请求");
      setFound(null);
      setQ("");
      setMsg("");
      void reload();
    } catch (e) {
      setHint(e instanceof Error ? e.message : "发送失败");
    }
  };

  const openDm = (u: UserPublic) => {
    const c = s.conversations.find((c) => c.kind === "dm" && c.peer?.id === u.id);
    setState({ tab: "chats", activeConv: c?.id ?? null });
  };

  const removeFriend = async (u: UserPublic) => {
    if (!confirm(`删除好友 ${u.display_name}？会话将变为只读。`)) return;
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
            placeholder="输入用户名精确搜索"
            value={q}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && search()}
          />
          <button onClick={search}>搜索</button>
        </div>
        {found && (
          <div className="row found">
            <div className="avatar">{found.display_name[0]?.toUpperCase()}</div>
            <div className="grow">
              <div>{found.display_name}</div>
              <div className="muted">@{found.username}</div>
            </div>
            <input
              placeholder="验证消息（可选）"
              value={msg}
              onChange={(e) => setMsg(e.target.value)}
            />
            <button className="primary" onClick={sendReq}>
              加好友
            </button>
          </div>
        )}
        {hint && <div className="muted">{hint}</div>}
      </section>

      <section className="card">
        <h3>新的朋友 {s.requests.incoming.length > 0 && `(${s.requests.incoming.length})`}</h3>
        {s.requests.incoming.length === 0 && s.requests.outgoing.length === 0 && (
          <div className="muted">暂无好友请求</div>
        )}
        {s.requests.incoming.map((r) => (
          <div className="row" key={r.id}>
            <div className="avatar">{r.user.display_name[0]?.toUpperCase()}</div>
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
            <div className="avatar">{r.user.display_name[0]?.toUpperCase()}</div>
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
            <div className="avatar">
              {f.user.display_name[0]?.toUpperCase()}
              {s.online[f.user.id] && <i className="online-dot" />}
            </div>
            <div className="grow">
              <div>{f.user.display_name}</div>
              <div className="muted">
                @{f.user.username} · {s.online[f.user.id] ? "在线" : "离线"}
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
