import { useState } from "react";
import { convs } from "../api";
import { setState, useApp } from "../store";
import Avatar from "./Avatar";

export default function NewGroupModal({ onClose }: { onClose: () => void }) {
  const s = useApp();
  const [name, setName] = useState("");
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);

  const toggle = (id: string) => {
    const next = new Set(picked);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setPicked(next);
  };

  const create = async () => {
    if (picked.size === 0) {
      setErr("至少选择一位好友");
      return;
    }
    setBusy(true);
    try {
      const d = await convs.createGroup(name.trim() || "群聊", [...picked]);
      const list = await convs.list();
      setState({ conversations: list, activeConv: d.id, tab: "chats" });
      onClose();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "创建失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>发起群聊</h3>
        <input
          placeholder="群名称（可留空）"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
        <div className="pick-list">
          {s.friends.length === 0 && <div className="muted">还没有好友，先去联系人页添加</div>}
          {s.friends.map((f) => (
            <label className="row pick" key={f.user.id}>
              <input
                type="checkbox"
                checked={picked.has(f.user.id)}
                onChange={() => toggle(f.user.id)}
              />
              <Avatar userId={f.user.id} name={f.user.display_name} avatarVersion={f.user.avatar_version} hasAvatar={f.user.has_avatar} size="sm" />
              <span>{f.user.display_name}</span>
            </label>
          ))}
        </div>
        {err && <div className="auth-err">{err}</div>}
        <div className="row end">
          <button onClick={onClose}>取消</button>
          <button className="primary" onClick={create} disabled={busy}>
            {busy ? "创建中…" : `创建（${picked.size}）`}
          </button>
        </div>
      </div>
    </div>
  );
}
