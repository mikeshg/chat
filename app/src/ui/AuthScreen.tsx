import { useState } from "react";
import { auth, getServer, saveSession, setServer } from "../api";
import { setState } from "../store";
import { startWs } from "../ws";

export default function AuthScreen() {
  const [mode, setMode] = useState<"login" | "register">("login");
  const [username, setUsername] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [server, setServerUrl] = useState(getServer());
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setErr("");
    setBusy(true);
    try {
      setServer(server);
      const r =
        mode === "login"
          ? await auth.login(username.trim(), password)
          : await auth.register(username.trim(), password, displayName.trim() || undefined);
      saveSession(r);
      setState({ me: r.user });
      void startWs();
    } catch (ex) {
      setErr(ex instanceof Error ? ex.message : "请求失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="auth-wrap">
      <form className="auth-card" onSubmit={submit}>
        <h1 className="auth-title">Chat</h1>
        <div className="auth-tabs">
          <button
            type="button"
            className={mode === "login" ? "on" : ""}
            onClick={() => setMode("login")}
          >
            登录
          </button>
          <button
            type="button"
            className={mode === "register" ? "on" : ""}
            onClick={() => setMode("register")}
          >
            注册
          </button>
        </div>
        <input
          placeholder="用户名（3-20 位字母数字下划线）"
          value={username}
          onChange={(e) => setUsername(e.target.value)}
          autoComplete="username"
          required
        />
        {mode === "register" && (
          <input
            placeholder="昵称（可留空，之后可改）"
            value={displayName}
            onChange={(e) => setDisplayName(e.target.value)}
          />
        )}
        <input
          type="password"
          placeholder="密码（至少 6 位）"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoComplete={mode === "login" ? "current-password" : "new-password"}
          required
        />
        <input
          placeholder="服务器地址（同源部署可留空，如 http://192.168.1.5:8080）"
          value={server}
          onChange={(e) => setServerUrl(e.target.value)}
        />
        {err && <div className="auth-err">{err}</div>}
        <button className="primary" disabled={busy}>
          {busy ? "请稍候…" : mode === "login" ? "登录" : "注册并登录"}
        </button>
      </form>
    </div>
  );
}
