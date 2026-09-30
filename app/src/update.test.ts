import { describe, expect, it } from "vitest";
import { isNewer } from "./update";

describe("isNewer", () => {
  it.each([
    ["0.2.0", "0.1.0", true],
    ["1.2.10", "1.2.9", true],
    ["1.0.0", "1.0.0", false],
    ["0.9.9", "1.0.0", false],
    ["1.1", "1.0.5", true],
    ["1.0.0", "1.0", false],
  ])("isNewer(%s, %s) = %s", (a, b, want) => {
    expect(isNewer(a, b)).toBe(want);
  });
});
