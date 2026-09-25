import { afterEach, describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import PollCard, { schedulePollDeadline } from "./PollCard";
import type { ConversationView, MessageView } from "../types";

const conv: ConversationView = { id: "c", kind: "group", name: "C", owner_id: null, peer: null, member_count: 2, last_seq: 1, unread_count: 0, has_mention: false, last_message: null, created_at: "2026-01-01T00:00:00Z" };
const message: MessageView = { id: "m", conversation_id: "c", seq: 1, sender_id: "a", sender_name: "A", sender_username: "a", kind: "poll", content: "Q", attachment: null, mentions: [], created_at: "2026-01-01T00:00:00Z", poll: { question: "Q", allow_multiple: false, anonymous: false, closed_at: null, expired: false, expires_at: "2026-09-25T10:00:00Z", version: 0, total_voters: 0, my_option_ids: ["one"], options: [{ id: "one", index: 0, label: "One", count: 0, voters: [] }] } };
afterEach(() => vi.useRealTimers());

describe("poll deadline", () => {
  it("fires exactly at a distant deadline without timeout overflow and cancels on cleanup", () => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
    const expired = vi.fn();
    const cancel = schedulePollDeadline(new Date(3_000_000_000).toISOString(), expired);
    vi.advanceTimersByTime(2_147_483_647);
    expect(expired).not.toHaveBeenCalled();
    vi.advanceTimersByTime(852_516_352);
    expect(expired).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(expired).toHaveBeenCalledTimes(1);
    cancel();
    const cleanup = schedulePollDeadline(new Date(3_000_001_000).toISOString(), expired);
    cleanup();
    vi.advanceTimersByTime(1000);
    expect(expired).toHaveBeenCalledTimes(1);
  });

  it("disables expired options even before the server snapshot marks expired", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-09-25T10:00:00Z"));
    const html = renderToStaticMarkup(<PollCard m={message} conv={conv} members={[]} busy={false} setBusy={() => {}} onErr={() => {}} />);
    expect(html).toContain('disabled=""');
    expect(html).toContain("已截止");
    expect(html).not.toContain("撤票");
  });
});
