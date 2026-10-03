import { useState } from "react";
import { auth, getServer } from "../../api";
import { setState, useApp } from "../../store";
import { stopWs } from "../../ws";
import AutostartSetting from "./AutostartSetting";
import NotifySettings from "./NotifySettings";
import PushSettings from "./PushSettings";
import ThemeSetting from "./ThemeSetting";
import SecuritySettings from "./SecuritySettings";
import StorageUsage from "./StorageUsage";
import AvatarSetting from "./AvatarSetting";
import BotSettings from "./BotSettings";
import UpdateSetting from "./UpdateSetting";
import SetRow, { SetGroup } from "./SetRow";

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
      <div className="me-page">
        <h2>我</h2>
        <section className="card me-profile">
          <AvatarSetting />
          <div className="grow">
            <div className="me-name">{s.me?.display_name}</div>
            <div className="muted">@{s.me?.username}</div>
            <div className="row">
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="昵称"
                maxLength={50}
              />
              <button
                className="primary"
                disabled={!name.trim() || name.trim() === s.me?.display_name}
                onClick={save}
              >
                保存昵称
              </button>
            </div>
            {hint && <div className="muted">{hint}</div>}
          </div>
        </section>

        <SetGroup title="通用">
          <UpdateSetting />
          <AutostartSetting />
          <ThemeSetting />
          <StorageUsage />
          <SetRow label="服务器" desc={getServer() || "同源"}>
            <span className="set-value">
              <span className={`dot${s.wsReady ? " on" : ""}`} />
              {s.wsReady ? "已连接" : "未连接"}
            </span>
          </SetRow>
        </SetGroup>

        <SetGroup title="通知">
          <NotifySettings />
          <PushSettings />
        </SetGroup>

        <SetGroup title="机器人">
          <BotSettings />
        </SetGroup>

        <SetGroup title="账号与安全">
          <SecuritySettings />
        </SetGroup>

        <section className="card set-group set-logout-card">
          <button className="set-logout" onClick={logout}>
            退出登录
          </button>
        </section>
      </div>
    </main>
  );
}
