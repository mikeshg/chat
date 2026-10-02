import { useState } from "react";
import { ApiError, convs } from "../../api";
import { refreshCatalogs } from "../../ws";
import type { AnnouncementView } from "../../types";
import {
  isNewAnnouncement,
  readAnnouncementSeen,
  writeAnnouncementSeen,
} from "./announcement";
import AnnouncementEditor from "./AnnouncementEditor";

/** 公告第一行（截断由 CSS 负责） */
export function announcementPreview(text: string): string {
  return text.split("\n", 1)[0];
}

export default function AnnouncementBar({
  convId,
  announcement,
  canEdit,
}: {
  convId: string;
  announcement: AnnouncementView | null | undefined;
  canEdit: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [editing, setEditing] = useState(false);
  const [seen, setSeen] = useState(() => readAnnouncementSeen(convId));
  const text = announcement?.text ?? "";
  if (!text && !editing) return null;

  const fresh =
    !!announcement?.updated_at && isNewAnnouncement(announcement.updated_at, seen);
  const markSeen = () => {
    if (!announcement?.updated_at) return;
    writeAnnouncementSeen(convId, announcement.updated_at);
    setSeen(announcement.updated_at);
  };

  return (
    <div className={`ann-bar ${fresh ? "ann-new" : ""}`}>
      <button
        type="button"
        className="ann-toggle"
        onClick={() => {
          const next = !open;
          setOpen(next);
          if (next) markSeen();
          if (!next) setEditing(false);
        }}
      >
        <span>📢</span>
        <span className="ann-preview">{announcementPreview(text)}</span>
        {fresh && <span className="ann-badge">新公告</span>}
      </button>
      {open && (
        <div className="ann-body">
          {editing ? (
            <AnnouncementEditor
              initial={text}
              onCancel={() => setEditing(false)}
              onSave={async (next) => {
                await convs.setAnnouncement(convId, next);
                await refreshCatalogs();
                setEditing(false);
                if (!next) setOpen(false);
              }}
            />
          ) : (
            <>
              <pre className="ann-text">{text}</pre>
              <div className="ann-meta">
                <span>
                  {announcement?.updated_by_name
                    ? `由 ${announcement.updated_by_name} 更新于 ${
                        announcement.updated_at
                          ? new Date(announcement.updated_at).toLocaleString()
                          : ""
                      }`
                    : ""}
                </span>
                {canEdit && (
                  <button type="button" onClick={() => setEditing(true)}>
                    编辑
                  </button>
                )}
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}

export function announcementError(e: unknown): string {
  return e instanceof ApiError || e instanceof Error ? e.message : "保存失败";
}
