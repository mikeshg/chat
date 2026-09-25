import { useState } from "react";
import { auth, getServer } from "../api";
import { setState, useApp } from "../store";
import { stopWs } from "../ws";
import NotifySettings from "./NotifySettings";
import ThemeSetting from "./ThemeSetting";

export default function MePanel() {
  const s = useApp();
  const [name, setName] = useState(s.me?.display_name ?? "");
  const [hint, setHint] = useState("");

  const save = async () => {
    try {
      const me = await auth.updateMe(name.trim());
      setState({ me });
      setHint("已保存");
    } catch (e) {
      setHint(e instanceof Error ? e.message : "保存失败");
    }
  };

  const logout = () => {
    auth.logout();
    stopWs();
    window.dispatchEvent(new Event("chat:logout"));
  };

  return (
    <main className="contacts">
      <h2>我</h2>
      <section className="card">
        <div className="row">
          <div className="avatar lg">{s.me?.display_name[0]?.toUpperCase()}</div>
          <div className="grow">
            <div>
              <b>{s.me?.display_name}</b>
            </div>
            <div className="muted">@{s.me?.username}</div>
          </div>
        </div>
        <div className="row">
          <input
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="昵称"
            maxLength={50}
          />
          <button className="primary" onClick={save}>
            保存昵称
          </button>
        </div>
        {hint && <div className="muted">{hint}</div>}
      </section>

      <NotifySettings />
      <ThemeSetting />

      <section className="card">
        <h3>连接</h3>
        <div className="muted">
          服务器：{getServer() || "同源"} · 实时通道：{s.wsReady ? "已连接" : "未连接"}
        </div>
      </section>

      <section className="card">
        <button className="danger" onClick={logout}>
          退出登录
        </button>
      </section>
    </main>
  );
}
