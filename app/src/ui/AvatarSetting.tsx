import { useEffect, useRef, useState } from "react";
import { auth } from "../api";
import { cropAvatarFile } from "../avatar";
import { setState, useApp } from "../store";
import Avatar from "./Avatar";

/** 「我」页头像：点击选图 → 预览 → 确认上传，或移除。 */
export default function AvatarSetting() {
  const s = useApp();
  const inputRef = useRef<HTMLInputElement>(null);
  const [preview, setPreview] = useState<string | null>(null);
  const [blob, setBlob] = useState<Blob | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  useEffect(() => {
    return () => {
      if (preview) URL.revokeObjectURL(preview);
    };
  }, [preview]);

  const clearPreview = () => {
    if (preview) URL.revokeObjectURL(preview);
    setPreview(null);
    setBlob(null);
    if (inputRef.current) inputRef.current.value = "";
  };

  const onPick = async (file: File | undefined) => {
    if (!file) return;
    setErr("");
    setBusy(true);
    try {
      const next = await cropAvatarFile(file);
      if (preview) URL.revokeObjectURL(preview);
      setBlob(next);
      setPreview(URL.createObjectURL(next));
    } catch (e) {
      setErr(e instanceof Error ? e.message : "无法处理这张图片");
    } finally {
      setBusy(false);
    }
  };

  const useIt = async () => {
    if (!blob) return;
    setBusy(true);
    setErr("");
    try {
      const me = await auth.uploadAvatar(blob);
      setState({ me });
      clearPreview();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "上传失败");
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    setBusy(true);
    setErr("");
    try {
      const me = await auth.removeAvatar();
      setState({ me });
      clearPreview();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "移除失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="avatar-setting">
      <button
        type="button"
        className="avatar-pick"
        title="更换头像"
        aria-label="更换头像"
        disabled={busy}
        onClick={() => inputRef.current?.click()}
      >
        {preview ? (
          <img className="avatar lg avatar-preview" src={preview} alt="" />
        ) : (
          <Avatar
            userId={s.me?.id}
            name={s.me?.display_name}
            avatarVersion={s.me?.avatar_version}
            hasAvatar={s.me?.has_avatar}
            size="lg"
          />
        )}
      </button>
      <input
        ref={inputRef}
        type="file"
        accept="image/png,image/jpeg,image/webp"
        hidden
        onChange={(e) => void onPick(e.target.files?.[0])}
      />
      <div className="avatar-actions">
        {preview ? (
          <>
            <button className="primary" disabled={busy} onClick={() => void useIt()}>
              {busy ? "上传中…" : "使用这张"}
            </button>
            <button disabled={busy} onClick={clearPreview}>
              取消
            </button>
          </>
        ) : (
          s.me?.has_avatar && (
            <button className="danger" disabled={busy} onClick={() => void remove()}>
              {busy ? "处理中…" : "移除头像"}
            </button>
          )
        )}
        {err && <div className="auth-err">{err}</div>}
      </div>
    </div>
  );
}
