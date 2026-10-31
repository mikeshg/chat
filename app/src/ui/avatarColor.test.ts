import { describe, expect, it } from "vitest";
import { avatarColor } from "./avatarColor";
import { initialOf } from "./Avatar";

describe("avatarColor", () => {
  it("same id → same color; different ids usually differ", () => {
    expect(avatarColor("u-1")).toBe(avatarColor("u-1"));
    expect(avatarColor("alice")).not.toBe(avatarColor("bob"));
  });
  it("empty falls back to first palette color", () => {
    expect(avatarColor(null)).toBe("#4F46E5");
    expect(avatarColor("")).toBe("#4F46E5");
  });
});

describe("initialOf", () => {
  it("CJK first char / Latin upper / empty ?", () => {
    expect(initialOf("张三")).toBe("张");
    expect(initialOf("alice")).toBe("A");
    expect(initialOf("  bob")).toBe("B");
    expect(initialOf("")).toBe("?");
    expect(initialOf(null)).toBe("?");
  });
});
