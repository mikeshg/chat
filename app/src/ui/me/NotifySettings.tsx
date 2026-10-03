import { useEffect, useState } from "react";
import {
  notifyEnabled,
  notifyPermission,
  notifyPreview,
  notifySupport,
  requestNotifyPermission,
  setNotifyEnabled,
  setNotifyPreview,
  showSystemNotification,
  type NotifyPermission,
} from "../../notify";
import SetRow from "./SetRow";

/** ===== V2 P1-1 ===== 「我」页通知设置卡片（桌面通知开关 / 预览开关 / 测试通知） */
export default function NotifySettings() {
  const support = notifySupport();
  const [enabled, setEnabled] = useState(notifyEnabled);
  const [preview, setPreview] = useState(notifyPreview);
  const [perm, setPerm] = useState<NotifyPermission>("default");
  const [busy, setBusy] = useState(false);
  const [hint, setHint] = useState("");

  useEffect(() => {
    if (support === "none") return;
    void notifyPermission()
      .then(setPerm)
      .catch(() => {});
  }, [support]);

  const toggleEnabled = async () => {
    if (support === "none" || busy) return;
    if (enabled) {
      setEnabled(false);
      setNotifyEnabled(false);
      setHint("");
      return;
    }
    setBusy(true);
    try {
      // 权限请求必须在点击事件里发起，浏览器不允许自动弹权限框
      const ok = await requestNotifyPermission();
      const p = await notifyPermission();
      setPerm(p);
      setEnabled(ok);
      setNotifyEnabled(ok);
      setHint(
        ok
          ? ""
          : p === "denied"
            ? "通知权限被拒绝，请在浏览器 / 系统设置里允许本站通知"
            : "未获得通知权限",
      );
    } finally {
      setBusy(false);
    }
  };

  const togglePreview = (v: boolean) => {
    setPreview(v);
    setNotifyPreview(v);
  };

  const sendTest = async () => {
    await showSystemNotification({
      title: "Chat",
      body: notifyPreview() ? "这是一条测试通知" : "你收到一条新消息",
      convId: "test",
    });
    setHint("已发送（窗口在前台时部分系统不弹出，可切走再试）");
  };

  if (support === "none") return <SetRow label="桌面通知" desc="当前环境不支持系统通知" />;

  return (
    <>
      <SetRow
        label="桌面通知"
        desc={!enabled && hint ? hint : "窗口不在前台时弹出系统通知"}
        toggle={{
          checked: enabled,
          disabled: busy,
          onChange: () => void toggleEnabled(),
        }}
      />
      <SetRow label="通知显示消息内容" toggle={{ checked: preview, onChange: togglePreview }} />
      {enabled && (
        <SetRow
          label="测试通知"
          desc={
            hint ||
            `权限：${perm === "granted" ? "已允许" : perm === "denied" ? "已拒绝" : "未请求"}`
          }
        >
          <button disabled={perm !== "granted"} onClick={() => void sendTest()}>
            发送
          </button>
        </SetRow>
      )}
    </>
  );
}
