import { describe, expect, it } from "vitest";
import { isNewAnnouncement } from "./announcement";

// ===== V2 P3-2 =====
describe("isNewAnnouncement", () => {
  const updated = "2026-09-26T08:00:00.000Z";

  it("is new when this device has never opened the announcement", () => {
    expect(isNewAnnouncement(updated, null)).toBe(true);
    expect(isNewAnnouncement(updated, "")).toBe(true);
  });

  it("is new only when the announcement is newer than the seen time", () => {
    expect(isNewAnnouncement(updated, "2026-09-26T07:59:59.000Z")).toBe(true);
    expect(isNewAnnouncement(updated, updated)).toBe(false);
    expect(isNewAnnouncement(updated, "2026-09-26T08:00:01.000Z")).toBe(false);
  });

  it("is not new when either timestamp cannot be parsed", () => {
    expect(isNewAnnouncement("not-a-date", null)).toBe(false);
    expect(isNewAnnouncement(updated, "yesterday")).toBe(false);
  });
});
