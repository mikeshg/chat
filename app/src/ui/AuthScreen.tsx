import { useEffect, useState } from "react";
import { syncAgentHost } from "../agentHost";
import { auth, getServer, saveSession, serverInfo, setServer } from "../api";
import { readDrafts } from "../drafts";
import { parseInviteParam } from "../invite";
import { setState } from "../store";
import type { RegistrationMode } from "../types";
import { startWs } from "../ws";

export default function AuthScreen() {
  const [mode, setMode] = useState<"login" | "register">(
    // ?invite=CODE → 直接进注册并预填邀请码
    () => (parseInviteParam(window.location.search) ? "register" : "login"),
  );
  const [inviteCode, setInviteCode] = useState(() =>
    parseInviteParam(window.location.search),
  );
  const [regMode, setRegMode] = useState<RegistrationMode>("open");
  const [username, setUsername] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [server, setServerUrl] = useState(getServer());
  const [err, setErr] = useState(() => {
    // 被踢下线（禁用/改密）的提示只显示一次
    const r = sessionStorage.getItem("chat_kick_reason");
    if (r) sessionStorage.removeItem("chat_kick_reason");
    return r ?? "";
  });
  const [busy, setBusy] = useState(false);

  // 挂载时 + 服务器地址输入变化（防抖 ~500ms）探测目标服务器的注册模式；
  // 探测失败按 open（serverInfo 内部已兜底）
  useEffect(() => {
    let stale = false;
    const t = setTimeout(() => {
      void serverInfo(server).then((i) => {
        if (!stale) setRegMode(i.registration_mode);
      });
    }, 500);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [server]);

  // 关闭注册时注册入口整体消失（含已在注册页的情况）
  const effMode = regMode === "closed" ? "login" : mode;

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setErr("");
    setBusy(true);
    try {
      setServer(server);
      const r =
        effMode === "login"
          ? await auth.login(username.trim(), password)
          : await auth.register(
              username.trim(),
              password,
              displayName.trim() || undefined,
              inviteCode.trim() || undefined,
            );
      saveSession(r);
      setState({ me: r.user, drafts: readDrafts(localStorage, r.user.id) });
      void startWs();
      void syncAgentHost(r.user);
    } catch (ex) {
      setErr(ex instanceof Error ? ex.message : "请求失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="auth-wrap">
      <form className="auth-card" onSubmit={submit}>
        <div className="auth-head">
          <h1 className="auth-title">Chat</h1>
          {regMode !== "closed" && (
            <button
              type="button"
              className="auth-switch"
              onClick={() => setMode(mode === "login" ? "register" : "login")}
            >
              {mode === "login" ? "注册 →" : "← 登录"}
            </button>
          )}
        </div>
        <input
          placeholder="用户名（3-20 位字母数字下划线）"
          value={username}
          onChange={(e) => setUsername(e.target.value)}
          autoComplete="username"
          required
        />
        {effMode === "register" && (
          <input
            placeholder="昵称（可留空，之后可改）"
            value={displayName}
            onChange={(e) => setDisplayName(e.target.value)}
          />
        )}
        {effMode === "register" && regMode === "invite" && (
          <input
            placeholder="邀请码"
            value={inviteCode}
            onChange={(e) => setInviteCode(e.target.value)}
            required
          />
        )}
        <input
          type="password"
          placeholder="密码（至少 6 位）"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoComplete={effMode === "login" ? "current-password" : "new-password"}
          required
        />
        <input
          placeholder="服务器地址（同源部署可留空，如 http://192.168.1.5:28080）"
          value={server}
          onChange={(e) => setServerUrl(e.target.value)}
        />
        {regMode === "closed" && <div className="auth-note">管理员已关闭注册</div>}
        {err && <div className="auth-err">{err}</div>}
        <button className="primary" disabled={busy}>
          {busy ? "请稍候…" : effMode === "login" ? "登录" : "注册并登录"}
        </button>
      </form>
    </div>
  );
}
