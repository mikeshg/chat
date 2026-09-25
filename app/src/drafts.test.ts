import { describe, expect, it } from "vitest";
import { draftsKey, readDrafts, writeDrafts } from "./drafts";

/** 内存版 localStorage（可按需注入坏数据/抛错） */
function memStorage(initial: Record<string, string> = {}) {
  const m = new Map(Object.entries(initial));
  return {
    getItem: (k: string) => (m.has(k) ? m.get(k)! : null),
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
    raw: m,
  };
}

// ===== V2 P1-3 =====
describe("草稿持久化（localStorage chat_drafts:<userId>）", () => {
  it("写入→读回：同一会话草稿往返一致，键名按用户隔离", () => {
    const st = memStorage();
    writeDrafts(st, "u1", { c1: "hello", c2: "a\nb" });
    expect(st.raw.get(draftsKey("u1"))).toBe('{"c1":"hello","c2":"a\\nb"}');
    expect(readDrafts(st, "u1")).toEqual({ c1: "hello", c2: "a\nb" });
  });

  it("按用户隔离：u1 的草稿对 u2 不可见，互写不串", () => {
    const st = memStorage();
    writeDrafts(st, "u1", { c1: "u1 draft" });
    expect(readDrafts(st, "u2")).toEqual({});
    writeDrafts(st, "u2", { c1: "u2 draft" });
    expect(readDrafts(st, "u1")).toEqual({ c1: "u1 draft" });
    expect(readDrafts(st, "u2")).toEqual({ c1: "u2 draft" });
  });

  it("损坏 JSON / 非对象值 → 按空处理", () => {
    for (const bad of ["{", "null", "[1,2]", '"str"', "42", "true"]) {
      const st = memStorage({ [draftsKey("u1")]: bad });
      expect(readDrafts(st, "u1"), `input=${bad}`).toEqual({});
    }
  });

  it("读入时剔除非字符串/空串条目", () => {
    const st = memStorage({
      [draftsKey("u1")]: JSON.stringify({ a: 1, b: "x", c: "", d: null, e: {} }),
    });
    expect(readDrafts(st, "u1")).toEqual({ b: "x" });
  });

  it("storage 抛错不炸：读返回 {}，写静默失败", () => {
    const throwing = {
      getItem: () => { throw new Error("denied"); },
      setItem: () => { throw new Error("denied"); },
    };
    expect(readDrafts(throwing, "u1")).toEqual({});
    expect(() => writeDrafts(throwing, "u1", { c1: "x" })).not.toThrow();
  });
});
