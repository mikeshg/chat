import { describe, expect, it } from "vitest";
import { draftsKey, readDrafts, unsentText, writeDrafts } from "./drafts";
import { memStorage } from "./testStorage";

const A = "enc-a"; // 服务器分键段（真实值是 encodeURIComponent(规范化地址)）
const B = "enc-b";

// ===== V2 P1-3 / T5：草稿键带服务器段 =====
describe("草稿持久化（localStorage chat_drafts:<enc(server)>:<userId>）", () => {
  it("写入→读回：同一会话草稿往返一致，键名按服务器+用户隔离", () => {
    const st = memStorage();
    writeDrafts(st, A, "u1", { c1: "hello", c2: "a\nb" });
    expect(st.raw.get(draftsKey(A, "u1"))).toBe('{"c1":"hello","c2":"a\\nb"}');
    expect(readDrafts(st, A, "u1")).toEqual({ c1: "hello", c2: "a\nb" });
  });

  it("按用户隔离：u1 的草稿对 u2 不可见，互写不串", () => {
    const st = memStorage();
    writeDrafts(st, A, "u1", { c1: "u1 draft" });
    expect(readDrafts(st, A, "u2")).toEqual({});
    writeDrafts(st, A, "u2", { c1: "u2 draft" });
    expect(readDrafts(st, A, "u1")).toEqual({ c1: "u1 draft" });
    expect(readDrafts(st, A, "u2")).toEqual({ c1: "u2 draft" });
  });

  it("按服务器隔离：同一用户在 A/B 两服的草稿互不可见", () => {
    const st = memStorage();
    writeDrafts(st, A, "u1", { c1: "draft on A" });
    expect(readDrafts(st, B, "u1")).toEqual({});
    writeDrafts(st, B, "u1", { c1: "draft on B" });
    expect(readDrafts(st, A, "u1")).toEqual({ c1: "draft on A" });
    expect(readDrafts(st, B, "u1")).toEqual({ c1: "draft on B" });
  });

  it("损坏 JSON / 非对象值 → 按空处理", () => {
    for (const bad of ["{", "null", "[1,2]", '"str"', "42", "true"]) {
      const st = memStorage({ [draftsKey(A, "u1")]: bad });
      expect(readDrafts(st, A, "u1"), `input=${bad}`).toEqual({});
    }
  });

  it("读入时剔除非字符串/空串条目", () => {
    const st = memStorage({
      [draftsKey(A, "u1")]: JSON.stringify({ a: 1, b: "x", c: "", d: null, e: {} }),
    });
    expect(readDrafts(st, A, "u1")).toEqual({ b: "x" });
  });

  it("storage 抛错不炸：读返回 {}，写静默失败", () => {
    const throwing = {
      getItem: () => { throw new Error("denied"); },
      setItem: () => { throw new Error("denied"); },
    };
    expect(readDrafts(throwing, A, "u1")).toEqual({});
    expect(() => writeDrafts(throwing, A, "u1", { c1: "x" })).not.toThrow();
  });
});

describe("unsentText：发送完成只清已发出的部分", () => {
  it("未改动 → 清空", () => expect(unsentText("hello", "hello")).toBe(""));
  it("发送期间追加输入 → 只留新输入", () => expect(unsentText("hello next", "hello")).toBe("next"));
  it("改过已发内容 → 原样保留", () => expect(unsentText("hell0", "hello")).toBe("hell0"));
  it("只发附件（无文字）→ 不动输入框", () => expect(unsentText("typing", "")).toBe("typing"));
});
