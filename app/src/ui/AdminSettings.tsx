import { useCallback, useEffect, useRef, useState } from "react";
import { admin, getServer } from "../api";
import { inviteLink } from "../invite";
import type { InviteView, RegistrationMode } from "../types";

const MODES: { v: RegistrationMode; label: string }[] = [
  { v: "open", label: "开放注册" },
  { v: "invite", label: "仅限邀请" },
  { v: "closed", label: "关闭注册" },
];

function fmtTime(s: string | null) {
  return s ? new Date(s).toLocaleString() : "永久";
}

/** 状态标签：usable 由服务端算好，这里只补充不可用的原因 */
function inviteStatus(i: InviteView): { text: string; usable: boolean } {
  if (i.revoked_at) return { text: "已撤销", usable: false };
  if (i.expires_at && Date.parse(i.expires_at) <= Date.now())
    return { text: "已过期", usable: false };
  if (i.max_uses != null && i.uses >= i.max_uses) return { text: "已用完", usable: false };
  return { text: "可用", usable: i.usable };
}

export default function AdminSettings() {
  const [mode, setMode] = useState<RegistrationMode>("open");
  const [invites, setInvites] = useState<InviteView[]>([]);
  const [err, setErr] = useState("");
  const [saved, setSaved] = useState(false);
  const [switching, setSwitching] = useState(false);
  // V2 P2-1：每用户存储配额（MB 输入框字符串；0 = 不限）
  const [quota, setQuota] = useState("0");
  const [quotaSaving, setQuotaSaving] = useState(false);
  // 创建表单
  const [maxUses, setMaxUses] = useState<number | null>(1);
  const [expHours, setExpHours] = useState<number | null>(24 * 7);
  const [note, setNote] = useState("");
  const [creating, setCreating] = useState(false);
  const [copied, setCopied] = useState("");
  const copiedTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const load = useCallback(async () => {
    try {
      const [s, list] = await Promise.all([admin.settings(), admin.invites()]);
      setMode(s.registration_mode);
      setQuota(String(s.storage_quota_mb ?? 0));
      setInvites(list);
      setErr("");
    } catch (e) {
      setErr(e instanceof Error ? e.message : "加载失败");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const switchMode = async (v: RegistrationMode) => {
    if (v === mode || switching) return;
    setSwitching(true);
    setErr("");
    setSaved(false);
    try {
      const s = await admin.updateSettings({ registration_mode: v });
      setMode(s.registration_mode);
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "保存失败");
    } finally {
      setSwitching(false);
    }
  };

  // 存储配额：0-1048576 整数，0 = 不限；服务端也会再校验并返回文案
  const saveQuota = async () => {
    const q = Number(quota);
    if (!Number.isInteger(q) || q < 0 || q > 1048576) {
      setErr("配额必须是 0-1048576 的整数（0 = 不限）");
      return;
    }
    setQuotaSaving(true);
    setErr("");
    setSaved(false);
    try {
      const s = await admin.updateSettings({ storage_quota_mb: q });
      setQuota(String(s.storage_quota_mb ?? 0));
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "保存失败");
    } finally {
      setQuotaSaving(false);
    }
  };

  const create = async () => {
    if (creating) return;
    setCreating(true);
    setErr("");
    try {
      await admin.createInvite({
        max_uses: maxUses,
        expires_in_hours: expHours,
        note: note.trim() || undefined,
      });
      setNote("");
      await load();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "创建失败");
    } finally {
      setCreating(false);
    }
  };

  const flashCopied = (what: string) => {
    setCopied(what);
    if (copiedTimer.current) clearTimeout(copiedTimer.current);
    copiedTimer.current = setTimeout(() => setCopied(""), 2000);
  };

  const copy = async (text: string, what: string) => {
    try {
      await navigator.clipboard.writeText(text);
      flashCopied(`${what}已复制`);
    } catch {
      // 剪贴板不可用（非安全上下文/权限被拒）→ 弹出手动复制
      window.prompt(`${what}复制失败，请手动复制：`, text);
    }
  };

  const copyLink = (i: InviteView) => {
    const link = inviteLink(getServer(), window.location.origin, i.code);
    // 没有可用的服务器基址（Tauri 未配置）时退化为只复制码
    void copy(link || i.code, link ? "邀请链接" : "邀请码");
  };

  const revoke = async (i: InviteView) => {
    if (!confirm(`撤销邀请码 ${i.code}？已注册用户不受影响`)) return;
    try {
      await admin.revokeInvite(i.id);
      await load();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "撤销失败");
    }
  };

  return (
    <div className="card ad-settings">
      <h3>注册设置</h3>
      {err && <div className="auth-err">{err}</div>}
      <div className="row ad-mode">
        {MODES.map((m) => (
          <label key={m.v} className="ad-radio">
            <input
              type="radio"
              name="reg-mode"
              checked={mode === m.v}
              disabled={switching}
              onChange={() => void switchMode(m.v)}
            />
            {m.label}
          </label>
        ))}
        {switching && <span className="muted">保存中…</span>}
        {saved && <span className="muted ad-saved">已保存</span>}
      </div>

      <h3>存储配额</h3>
      <div className="row ad-quota">
        <input
          className="ad-quota-input"
          type="number"
          min={0}
          max={1048576}
          step={1}
          value={quota}
          disabled={quotaSaving}
          onChange={(e) => setQuota(e.target.value)}
        />
        <span className="muted">MB / 用户（0 = 不限，上限 1048576）</span>
        <button
          className="primary"
          disabled={quotaSaving}
          onClick={() => void saveQuota()}
        >
          {quotaSaving ? "保存中…" : "保存配额"}
        </button>
      </div>

      <h3>邀请码</h3>
      <div className="row ad-invite-form">
        <select
          value={maxUses === null ? "inf" : String(maxUses)}
          onChange={(e) =>
            setMaxUses(e.target.value === "inf" ? null : Number(e.target.value))
          }
        >
          <option value="1">1 次</option>
          <option value="5">5 次</option>
          <option value="inf">不限次数</option>
        </select>
        <select
          value={expHours === null ? "inf" : String(expHours)}
          onChange={(e) =>
            setExpHours(e.target.value === "inf" ? null : Number(e.target.value))
          }
        >
          <option value="24">1 天</option>
          <option value="168">7 天</option>
          <option value="inf">永久</option>
        </select>
        <input
          className="grow"
          placeholder="备注（可选，100 字内）"
          value={note}
          maxLength={100}
          onChange={(e) => setNote(e.target.value)}
        />
        <button className="primary" disabled={creating} onClick={() => void create()}>
          {creating ? "生成中…" : "生成邀请码"}
        </button>
      </div>
      {copied && <div className="muted">{copied}</div>}

      <div className="ad-table ad-invites">
        <div className="ad-invite-row ad-head">
          <span>邀请码</span>
          <span>备注</span>
          <span>已用/上限</span>
          <span>过期时间</span>
          <span>状态</span>
          <span>创建人</span>
          <span>操作</span>
        </div>
        {invites.map((i) => {
          const st = inviteStatus(i);
          return (
            <div key={i.id} className="ad-invite-row">
              <span className="inv-code">{i.code}</span>
              <span className="muted inv-note" title={i.note}>
                {i.note || "—"}
              </span>
              <span>
                {i.uses} / {i.max_uses ?? "∞"}
              </span>
              <span className="muted">{fmtTime(i.expires_at)}</span>
              <span>
                <em className={`ad-badge ${st.usable ? "ad-on" : "ad-off"}`}>{st.text}</em>
              </span>
              <span className="muted">{i.created_by_name ?? "—"}</span>
              <span className="ad-actions">
                {i.usable && (
                  <>
                    <button onClick={() => copyLink(i)}>复制链接</button>
                    <button onClick={() => void copy(i.code, "邀请码")}>复制邀请码</button>
                  </>
                )}
                {!i.revoked_at && (
                  <button className="danger" onClick={() => void revoke(i)}>
                    撤销
                  </button>
                )}
              </span>
            </div>
          );
        })}
        {invites.length === 0 && <div className="muted ad-empty">还没有邀请码</div>}
      </div>
    </div>
  );
}
