import { useState } from "react";
import { getServer } from "../api";
import { noteAvatarMiss, senderAvatar, useApp } from "../store";
import { avatarColor } from "./avatarColor";

type Size = "sm" | "md" | "lg";

export function avatarUrl(userId: string, version: number): string {
  return `${getServer()}/v1/users/${userId}/avatar?v=${version}`;
}

export function initialOf(name?: string | null): string {
  const ch = name?.trim()?.[0];
  return ch ? ch.toUpperCase() : "?";
}

/** 有头像用图片，加载失败或无头像回落首字母 + 稳定色。叠加元素（在线点）由调用方放进 children。 */
export default function Avatar({
  userId,
  name,
  avatarVersion,
  hasAvatar,
  size = "md",
  className = "",
  title,
  children,
}: {
  userId?: string | null;
  name?: string | null;
  avatarVersion?: number;
  hasAvatar?: boolean;
  size?: Size;
  className?: string;
  title?: string;
  children?: React.ReactNode;
}) {
  const stored = useApp().avatars[userId ?? ""];
  const picked = userId
    ? senderAvatar(stored, avatarVersion, hasAvatar)
    : { version: 0, has: false };
  const showImg = !!userId && picked.has && picked.version > 0;
  const [broken, setBroken] = useState<string | null>(null);
  const src = showImg ? avatarUrl(userId, picked.version) : "";
  const failed = broken === src;
  const fallback = !showImg || failed;
  const style = fallback
    ? { backgroundColor: avatarColor(userId || name), color: "#fff" }
    : undefined;

  return (
    <div
      className={`avatar ${size === "md" ? "" : size} ${className}`.trim()}
      title={title}
      style={style}
      data-testid={fallback ? "avatar-initial" : "avatar-img"}
    >
      {showImg && !failed ? (
        <img
          src={src}
          alt=""
          loading="lazy"
          onError={() => {
            setBroken(src);
            noteAvatarMiss(userId, picked.version);
          }}
        />
      ) : (
        initialOf(name)
      )}
      {children}
    </div>
  );
}
