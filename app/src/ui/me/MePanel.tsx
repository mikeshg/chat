import { useState } from "react";
import { auth } from "../../api";
import { setState, useApp } from "../../store";
import { stopWs } from "../../ws";
import { BellIcon, BotIcon, SlidersIcon, UserIcon } from "../icons";
import SettingsShell from "../SettingsShell";
import AutostartSetting from "./AutostartSetting";
import NotifySettings from "./NotifySettings";
import PushSettings from "./PushSettings";
import ThemeSetting from "./ThemeSetting";
import SecuritySettings from "./SecuritySettings";
import StorageUsage from "./StorageUsage";
import AvatarSetting from "./AvatarSetting";
import BotSettings from "./BotSettings";
import UpdateSetting from "./UpdateSetting";
import ServerSetting from "./ServerSetting";
import { SetGroup } from "./SetRow";

/** 「我」页：账号、通用、通知、机器人 */
export default function MePanel() {
  return (
    <SettingsShell
      sections={[
        { id: "general", label: "通用", icon: SlidersIcon, render: () => <GeneralSection /> },
        { id: "account", label: "账号与安全", icon: UserIcon, render: () => <AccountSection /> },
        {
          id: "notify",
          label: "通知",
          icon: BellIcon,
          render: () => (
            <SetGroup>
              <NotifySettings />
              <PushSettings />
            </SetGroup>
          ),
        },
        {
          id: "bots",
          label: "机器人",
          icon: BotIcon,
          render: () => (
            <SetGroup>
              <BotSettings />
            </SetGroup>
          ),
        },
      ]}
    />
  );
}

function AccountSection() {
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
    <>
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

      <SetGroup>
        <SecuritySettings />
      </SetGroup>

      <section className="card set-group set-logout-card">
        <button className="set-logout" onClick={logout}>
          退出登录
        </button>
      </section>
    </>
  );
}

function GeneralSection() {
  return (
    <SetGroup>
      <UpdateSetting />
      <AutostartSetting />
      <ThemeSetting />
      <StorageUsage />
      <ServerSetting />
    </SetGroup>
  );
}
