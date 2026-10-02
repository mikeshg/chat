import { describe, expect, it, vi, afterEach } from "vitest";
import { preparePollSubmission } from "./NewPollModal";
import type { CreatePollReq } from "../../types";

const draft: CreatePollReq = { question: "Question", options: ["One", "Two"], allow_multiple: false, anonymous: false, expires_at: "2026-09-25T10:00:00Z" };
afterEach(() => vi.useRealTimers());

describe("poll submission payload", () => {
  it("retries the identical logical request with its original ID and deadline after an uncertain result", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-09-25T09:00:00Z"));
    const submitted = preparePollSubmission(draft, null);
    vi.setSystemTime(new Date("2026-09-25T11:00:00Z"));
    expect(preparePollSubmission(draft, submitted)).toBe(submitted);
    const changed = preparePollSubmission({ ...draft, question: "New question", expires_at: null }, submitted);
    expect(changed.client_msg_id).not.toBe(submitted.client_msg_id);
  });

  it("counts Unicode codepoints and validates normalized options", () => {
    const submitted = preparePollSubmission({ ...draft, question: "𠀀".repeat(200), options: ["𠀀".repeat(100), " Two ", "Two"], expires_at: null }, null);
    expect(submitted.options).toHaveLength(2);
    expect(() => preparePollSubmission({ ...draft, question: "𠀀".repeat(201), expires_at: null }, null)).toThrow("问题需");
    expect(() => preparePollSubmission({ ...draft, options: ["𠀀".repeat(101), "Two"], expires_at: null }, null)).toThrow("选项每项");
  });
});
