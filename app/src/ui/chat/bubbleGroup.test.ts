import { describe, expect, it } from "vitest";
import { inBubbleGroup } from "./bubbleGroup";

describe("inBubbleGroup", () => {
  const a = { sender_id: "u1", created_at: "2026-01-01T12:00:00.000Z" };
  it("groups same sender within 2 minutes", () => {
    expect(
      inBubbleGroup(
        { sender_id: "u1", created_at: "2026-01-01T12:01:30.000Z" },
        a,
      ),
    ).toBe(true);
  });
  it("does not group past 2 minutes or different sender", () => {
    expect(
      inBubbleGroup(
        { sender_id: "u1", created_at: "2026-01-01T12:02:01.000Z" },
        a,
      ),
    ).toBe(false);
    expect(
      inBubbleGroup(
        { sender_id: "u2", created_at: "2026-01-01T12:00:30.000Z" },
        a,
      ),
    ).toBe(false);
    expect(inBubbleGroup(a, null)).toBe(false);
  });
});
