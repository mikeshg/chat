import { useState } from "react";
import { getServer } from "../api";

export function agentAvatarUrl(agentId: string, version: number): string {
  return `${getServer()}/v1/agents/${agentId}/avatar?v=${version}`;
}

/** 有头像显示图片，没有或加载失败时沿用 🤖。 */
export default function AgentAvatar({
  agentId,
  name,
  version,
  className = "",
  size = "md",
  children,
}: {
  agentId?: string | null;
  name?: string | null;
  version?: number;
  className?: string;
  size?: "sm" | "md" | "lg";
  children?: React.ReactNode;
}) {
  const show = !!agentId && (version ?? 0) > 0;
  const src = show ? agentAvatarUrl(agentId!, version!) : "";
  const [broken, setBroken] = useState<string | null>(null);
  const failed = broken === src;
  const cls = `avatar ${size === "md" ? "" : size} agent-avatar ${className}`.replace(/\s+/g, " ").trim();
  return (
    <div className={cls} title={name ?? "Agent"}>
      {show && !failed ? (
        <img src={src} alt="" loading="lazy" onError={() => setBroken(src)} />
      ) : (
        "🤖"
      )}
      {children}
    </div>
  );
}
