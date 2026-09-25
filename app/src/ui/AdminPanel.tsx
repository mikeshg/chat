import { useCallback, useEffect, useState } from "react";
import { admin } from "../api";
import { useApp } from "../store";
import type { AdminOverview, AdminUserView } from "../types";
import AdminSettings from "./AdminSettings";
import Avatar from "./Avatar";

const PAGE = 50;

function fmtBytes(n: number) {
  if (n >= 1 << 30) return `${(n / (1 << 30)).toFixed(1)} GB`;
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(1)} MB`;
  if (n >= 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${n} B`;
}

function fmtTime(s: string | null) {
  return s ? new Date(s).toLocaleString() : "—";
}

export default function AdminPanel() {
  const s = useApp();
  const [ov, setOv] = useState<AdminOverview | null>(null);
  const [q, setQ] = useState("");
  const [offset, setOffset] = useState(0);
  const [users, setUsers] = useState<AdminUserView[]>([]);
  const [err, setErr] = useState("");

  const load = useCallback(async () => {
    try {
      const [o, u] = await Promise.all([admin.overview(), admin.users(q, offset)]);
      setOv(o);
      setUsers(u);
      setErr("");
    } catch (e) {
      setErr(e instanceof Error ? e.message : "加载失败");
    }
  }, [q, offset]);

  useEffect(() => {
    void load();
  }, [load]);

  const act = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
      await load();
    } catch (e) {
      alert(e instanceof Error ? e.message : "操作失败");
    }
  };

  const resetPw = (u: AdminUserView) => {
    const pw = window.prompt(`为 ${u.display_name}（@${u.username}）设置新密码（6-72 位）：`);
    if (!pw) return;
    void act(() => admin.resetPassword(u.id, pw));
  };

  const stat = (label: string, v: string | number) => (
    <div className="ad-stat">
      <b>{v}</b>
      <span>{label}</span>
    </div>
  );

  return (
    <div className="contacts admin-panel">
      <h2>后台管理</h2>
      {err && <div className="auth-err">{err}</div>}

      {ov && (
        <div className="ad-cards">
          {stat("用户总数", ov.user_count)}
          {stat("管理员", ov.admin_count)}
          {stat("已禁用", ov.disabled_count)}
          {stat("在线用户 / 连接", `${ov.online_users} / ${ov.online_conns}`)}
          {stat("消息总数", ov.message_count)}
          {stat("会话（私聊/群）", `${ov.dm_count} / ${ov.group_count}`)}
          {stat("附件", `${ov.attachment_count}（${fmtBytes(ov.attachment_bytes)}）`)}
        </div>
      )}

      <AdminSettings />

      <div className="card">
        <div className="ad-toolbar">
          <input
            placeholder="搜索用户名/昵称"
            value={q}
            onChange={(e) => {
              setQ(e.target.value);
              setOffset(0);
            }}
          />
          <button onClick={() => void load()}>刷新</button>
        </div>
        <div className="ad-table">
          <div className="ad-row ad-head">
            <span>用户</span>
            <span>角色/状态</span>
            <span>注册时间</span>
            <span>最后在线</span>
            <span>操作</span>
          </div>
          {users.map((u) => (
            <div key={u.id} className="ad-row">
              <span className="ad-user">
                <Avatar className="ad-avatar" userId={u.id} name={u.display_name} avatarVersion={u.avatar_version} hasAvatar={u.has_avatar} size="sm" />
                <span>
                  <b>{u.display_name}</b>
                  <small>@{u.username}</small>
                </span>
              </span>
              <span className="ad-flags">
                {u.role === "admin" && <em className="ad-badge ad-admin">管理员</em>}
                {u.disabled_at && <em className="ad-badge ad-off">已禁用</em>}
                {u.online && <em className="ad-badge ad-on">在线</em>}
                {!u.online && !u.disabled_at && u.role !== "admin" && (
                  <em className="ad-badge">普通用户</em>
                )}
              </span>
              <span className="muted">{fmtTime(u.created_at)}</span>
              <span className="muted">{u.online ? "现在" : fmtTime(u.last_seen_at)}</span>
              <span className="ad-actions">
                {u.id !== s.me?.id && (
                  <>
                    {u.role === "user" ? (
                      <button onClick={() => void act(() => admin.setRole(u.id, "admin"))}>
                        设为管理员
                      </button>
                    ) : (
                      <button
                        onClick={() => {
                          if (confirm(`撤销 ${u.display_name} 的管理员权限？`))
                            void act(() => admin.setRole(u.id, "user"));
                        }}
                      >
                        撤销管理员
                      </button>
                    )}
                    {u.disabled_at ? (
                      <button onClick={() => void act(() => admin.enable(u.id))}>启用</button>
                    ) : (
                      u.role !== "admin" && (
                        <button
                          className="danger"
                          onClick={() => {
                            if (confirm(`禁用 ${u.display_name}？其将被立即踢下线且无法登录`))
                              void act(() => admin.disable(u.id));
                          }}
                        >
                          禁用
                        </button>
                      )
                    )}
                  </>
                )}
                <button onClick={() => resetPw(u)}>重置密码</button>
              </span>
            </div>
          ))}
          {users.length === 0 && <div className="muted ad-empty">没有匹配的用户</div>}
        </div>
        <div className="ad-pager">
          <button disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE))}>
            上一页
          </button>
          <span className="muted">{offset + 1} - {offset + users.length}</span>
          <button disabled={users.length < PAGE} onClick={() => setOffset(offset + PAGE)}>
            下一页
          </button>
        </div>
      </div>
    </div>
  );
}
