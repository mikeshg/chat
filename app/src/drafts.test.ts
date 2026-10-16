import { describe, expect, it } from "vitest";
import {
  clearReuseOnAck,
  draftsKey,
  failReuseSend,
  readDrafts,
  unsentText,
  writeDrafts,
  type DraftReuse,
} from "./drafts";
import { memStorage } from "./testStorage";

const A = "enc-a"; // 服务器分键段（真实值是 encodeURIComponent(规范化地址)）
const B = "enc-b";

function reuse(patch: Partial<DraftReuse> = {}): DraftReuse {
  return {
    attachment_id: "att1",
    file_name: "a.png",
    mime: "image/png",
    size: 10,
    kind: "image",
    conversation_id: "c1",
    ...patch,
  };
}

// ===== V2 P1-3 / T5：草稿键带服务器段 =====
describe("草稿持久化（localStorage chat_drafts:<enc(server)>:<userId>）", () => {
  it("写入→读回：同一会话草稿往返一致，键名按服务器+用户隔离", () => {
    const st = memStorage();
    writeDrafts(st, A, "u1", { c1: { text: "hello" }, c2: { text: "a\nb" } });
    // 无 reuse 的条目仍写旧格式纯文本（降级兼容）
    expect(st.raw.get(draftsKey(A, "u1"))).toBe('{"c1":"hello","c2":"a\\nb"}');
    expect(readDrafts(st, A, "u1")).toEqual({ c1: { text: "hello" }, c2: { text: "a\nb" } });
  });

  it("按用户隔离：u1 的草稿对 u2 不可见，互写不串", () => {
    const st = memStorage();
    writeDrafts(st, A, "u1", { c1: { text: "u1 draft" } });
    expect(readDrafts(st, A, "u2")).toEqual({});
    writeDrafts(st, A, "u2", { c1: { text: "u2 draft" } });
    expect(readDrafts(st, A, "u1")).toEqual({ c1: { text: "u1 draft" } });
    expect(readDrafts(st, A, "u2")).toEqual({ c1: { text: "u2 draft" } });
  });

  it("按服务器隔离：同一用户在 A/B 两服的草稿互不可见", () => {
    const st = memStorage();
    writeDrafts(st, A, "u1", { c1: { text: "draft on A" } });
    expect(readDrafts(st, B, "u1")).toEqual({});
    writeDrafts(st, B, "u1", { c1: { text: "draft on B" } });
    expect(readDrafts(st, A, "u1")).toEqual({ c1: { text: "draft on A" } });
    expect(readDrafts(st, B, "u1")).toEqual({ c1: { text: "draft on B" } });
  });

  it("损坏 JSON / 非对象值 → 按空处理", () => {
    for (const bad of ["{", "null", "[1,2]", '"str"', "42", "true"]) {
      const st = memStorage({ [draftsKey(A, "u1")]: bad });
      expect(readDrafts(st, A, "u1"), `input=${bad}`).toEqual({});
    }
  });

  it("读入时剔除非字符串/空条目；旧版纯文本格式照常解析", () => {
    const st = memStorage({
      [draftsKey(A, "u1")]: JSON.stringify({ a: 1, b: "x", c: "", d: null, e: {} }),
    });
    expect(readDrafts(st, A, "u1")).toEqual({ b: { text: "x" } });
  });

  it("storage 抛错不炸：读返回 {}，写静默失败", () => {
    const throwing = {
      getItem: () => { throw new Error("denied"); },
      setItem: () => { throw new Error("denied"); },
    };
    expect(readDrafts(throwing, A, "u1")).toEqual({});
    expect(() => writeDrafts(throwing, A, "u1", { c1: { text: "x" } })).not.toThrow();
  });
});

// ===== MM2：复用附件 chip 随草稿持久化 =====
describe("复用附件 chip（draft.reuse）", () => {
  it("带 reuse 的条目按对象写回，读回一致", () => {
    const st = memStorage();
    writeDrafts(st, A, "u1", { c1: { text: "", reuse: reuse() } });
    expect(readDrafts(st, A, "u1")).toEqual({ c1: { text: "", reuse: reuse() } });
    expect(readDrafts(st, A, "u1").c1.reuse?.attachment_id).toBe("att1");
  });

  it("reuse 字段形态不对被剔除", () => {
    const st = memStorage({
      [draftsKey(A, "u1")]: JSON.stringify({
        c1: { text: "t", reuse: { attachment_id: 1 } },
        c2: { reuse: reuse() },
      }),
    });
    const d = readDrafts(st, A, "u1");
    expect(d.c1).toEqual({ text: "t" });
    expect(d.c2.reuse?.attachment_id).toBe("att1");
  });

  it("failReuseSend：文字退回草稿、chip 标过期且清在途标记", () => {
    const d0 = { c1: { text: "", reuse: reuse({ pendingClientMsgId: "cm1" }) } };
    const d = failReuseSend(d0, "c1", "att1", "补发文字");
    expect(d.c1.text).toBe("补发文字");
    expect(d.c1.reuse?.expired).toBe(true);
    expect(d.c1.reuse?.pendingClientMsgId).toBeUndefined();
  });

  it("failReuseSend：已有文字时把发出内容拼回尾部；chip 不匹配时不误标", () => {
    const d0 = { c1: { text: "旧输入", reuse: reuse() } };
    const d = failReuseSend(d0, "c1", "other-att", "补发文字");
    expect(d.c1.text).toBe("旧输入\n补发文字");
    expect(d.c1.reuse?.expired).toBeUndefined();
  });

  it("clearReuseOnAck：成功发送才摘 chip，文字草稿不动", () => {
    const d0 = { c1: { text: "留着的字", reuse: reuse() } };
    const d = clearReuseOnAck(d0, "c1", "att1");
    expect(d.c1).toEqual({ text: "留着的字" });
    // 无文字时整条删掉
    expect(clearReuseOnAck({ c1: { text: "", reuse: reuse() } }, "c1", "att1")).toEqual({});
    // chip id 不匹配 → 不动
    expect(clearReuseOnAck(d0, "c1", "nope")).toBe(d0);
  });
});

describe("unsentText：发送完成只清已发出的部分", () => {
  it("未改动 → 清空", () => expect(unsentText("hello", "hello")).toBe(""));
  it("发送期间追加输入 → 只留新输入", () => expect(unsentText("hello next", "hello")).toBe("next"));
  it("改过已发内容 → 原样保留", () => expect(unsentText("hell0", "hello")).toBe("hell0"));
  it("只发附件（无文字）→ 不动输入框", () => expect(unsentText("typing", "")).toBe("typing"));
});
