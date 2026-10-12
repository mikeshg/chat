import { describe, expect, it } from "vitest";
import {
  authKey,
  encServer,
  kickReasonKey,
  listServers,
  migrateLegacyStorage,
  needsInsecureConfirm,
  normalizeServerUrl,
  readAuth,
  resolveServerInput,
  removeServerData,
  removeServerEntry,
  upsertServer,
  writeAuth,
  type ServerEntry,
} from "./servers";
import { memStorage } from "./testStorage";

const user = (id: string) => ({
  id,
  username: id,
  display_name: id,
  role: "user" as const,
  last_seen_at: null,
});

// ===== T5：服务器地址规范化 =====
describe("normalizeServerUrl", () => {
  it.each([
    ["https://chat.287152.xyz", "https://chat.287152.xyz"],
    ["  https://a.example/  ", "https://a.example"],
    ["chat.287152.xyz", "http://chat.287152.xyz"],
    ["192.168.1.5:28080", "http://192.168.1.5:28080"],
    ["localhost:29051", "http://localhost:29051"],
    ["HTTP://EXAMPLE.COM/", "http://example.com"],
    ["http://a:80", "http://a"],
    ["https://a:443", "https://a"],
    ["http://a:8080", "http://a:8080"],
    ["https://a/sub/path/", "https://a/sub/path"],
    ["https://a/", "https://a"],
    ["https://a///", "https://a"],
  ])("%j → %j", (input, want) => {
    expect(normalizeServerUrl(input)).toBe(want);
  });

  it("http 与 https 是不同条目；localhost 与 127.0.0.1 不合并", () => {
    expect(normalizeServerUrl("http://a")).not.toBe(normalizeServerUrl("https://a"));
    expect(normalizeServerUrl("http://localhost")).not.toBe(
      normalizeServerUrl("http://127.0.0.1"),
    );
  });

  it("规范化幂等：再喂一次结果不变", () => {
    const once = normalizeServerUrl("HTTPS://A.Example:443/x/");
    expect(normalizeServerUrl(once!)).toBe(once);
  });

  it.each(["", "   ", "ftp://a", "ws://a", "http://", "://a", "javascript:alert(1)"])(
    "非法输入 %j → null",
    (input) => {
      expect(normalizeServerUrl(input)).toBeNull();
    },
  );
});

// ===== T6：登录提交时解析地址框输入 =====
// 地址框里的输入在提交前不必先落盘：提交一律用「当前输入」解析，
// 保证改完地址直接回车/点登录就连新地址，不靠 blur 保存。
describe("resolveServerInput", () => {
  const web = { desktop: false, origin: "http://localhost:29061" };

  it("输入新地址 → 返回规范化后的新地址（未保存的改动也生效）", () => {
    expect(resolveServerInput("localhost:29062", web)).toEqual({
      ok: true,
      url: "http://localhost:29062",
    });
    expect(resolveServerInput("  HTTPS://B.Example:443/x/ ", web)).toEqual({
      ok: true,
      url: "https://b.example/x",
    });
  });

  it("非空但非法 → 报错，不返回地址", () => {
    for (const bad of ["http://[bad", "ftp://a", "://a", "javascript:x"]) {
      const r = resolveServerInput(bad, web);
      expect(r.ok, `input=${bad}`).toBe(false);
      if (!r.ok) expect(r.err).toContain("服务器地址");
    }
  });

  it("Web 端留空 → 同源地址；桌面端留空 → 提示必填", () => {
    expect(resolveServerInput("", web)).toEqual({ ok: true, url: "http://localhost:29061" });
    expect(resolveServerInput("   ", web)).toEqual({ ok: true, url: "http://localhost:29061" });
    const r = resolveServerInput("", { desktop: true });
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.err).toContain("请填写服务器地址");
  });
});

describe("needsInsecureConfirm", () => {
  it("仅 Web https 页面连 http 服务器时需要确认", () => {
    expect(needsInsecureConfirm(false, "https:", "http://a")).toBe(true);
    expect(needsInsecureConfirm(false, "https:", "https://a")).toBe(false);
    expect(needsInsecureConfirm(false, "http:", "http://a")).toBe(false);
    expect(needsInsecureConfirm(true, "https:", "http://a")).toBe(false);
  });
});

describe("encServer", () => {
  it("编码后不含 : 或 /，键名分段不会错位", () => {
    const e = encServer("http://a:8080/x");
    expect(e).not.toMatch(/[:/]/);
    expect(e).toBe(encodeURIComponent("http://a:8080/x"));
  });
});

// ===== T5：服务器列表 =====
describe("服务器列表 chat_servers", () => {
  it("新增条目进列表并持久化，最近使用在前", () => {
    const st = memStorage();
    upsertServer(st, "http://a", {});
    upsertServer(st, "http://b", {});
    const list = listServers(st);
    expect(list.map((s) => s.url)).toEqual(["http://b", "http://a"]);
    expect(JSON.parse(st.raw.get("chat_servers")!)).toHaveLength(2);
  });

  it("同一个地址 upsert 不重复，且把它顶到最前", () => {
    const st = memStorage();
    upsertServer(st, "http://a", {});
    upsertServer(st, "http://b", {});
    upsertServer(st, "http://a", { username: "u1" });
    const list = listServers(st);
    expect(list.map((s) => s.url)).toEqual(["http://a", "http://b"]);
    expect(list[0].username).toBe("u1");
    expect(list).toHaveLength(2);
  });

  it("removeServerEntry 删除条目", () => {
    const st = memStorage();
    upsertServer(st, "http://a", {});
    upsertServer(st, "http://b", {});
    removeServerEntry(st, "http://a");
    expect(listServers(st).map((s) => s.url)).toEqual(["http://b"]);
  });

  it("损坏 JSON / 非数组 → 空列表", () => {
    for (const bad of ["{", "null", '"str"', "42", '{"a":1}']) {
      const st = memStorage({ chat_servers: bad });
      expect(listServers(st), `input=${bad}`).toEqual([]);
    }
  });

  it("条目里 url 缺失/非法的被过滤", () => {
    const st = memStorage({
      chat_servers: JSON.stringify([
        { url: "http://a", lastUsedAt: 1 },
        { url: 42, lastUsedAt: 9 },
        { nope: true },
        null,
      ] satisfies unknown[]),
    });
    expect(listServers(st).map((s) => s.url)).toEqual(["http://a"]);
  });
});

// ===== T5：按服务器的登录凭据 =====
describe("chat_auth:<enc> 凭据", () => {
  it("写入→读回；不同服务器互不可见", () => {
    const st = memStorage();
    writeAuth(st, encServer("http://a"), {
      access: "aa",
      refresh: "ra",
      user: user("u-a"),
    });
    writeAuth(st, encServer("http://b"), {
      access: "ab",
      refresh: "rb",
      user: user("u-b"),
    });
    expect(readAuth(st, encServer("http://a"))?.refresh).toBe("ra");
    expect(readAuth(st, encServer("http://b"))?.refresh).toBe("rb");
    expect(readAuth(st, encServer("http://c"))).toBeNull();
    expect(st.raw.get(authKey(encServer("http://a")))).toBeTruthy();
  });

  it("损坏/缺字段 → null", () => {
    for (const bad of ["{", "null", "42", '"s"', "[]", '{"refresh":1}', '{"refresh":"r"}']) {
      const st = memStorage({ [authKey(encServer("http://a"))]: bad });
      expect(readAuth(st, encServer("http://a")), `input=${bad}`).toBeNull();
    }
  });
});

// ===== T5：删除服务器清掉它的全部本地数据 =====
describe("removeServerData", () => {
  it("清该服务器的 auth/cache/outbox/drafts/kick，精确按 enc 前缀，不误伤其它服务器", () => {
    const a = encServer("http://a");
    const b = encServer("http://ab"); // 前缀相近：a 是 ab 的前缀，靠末尾冒号区分
    const st = memStorage({
      [authKey(a)]: "{}",
      [`chat_cache:${a}:u1`]: "c1",
      [`chat_cache:${a}:u2`]: "c2",
      [`chat_outbox:${a}:u1`]: "o1",
      [`chat_drafts:${a}:u1`]: "d1",
      [authKey(b)]: "{}",
      [`chat_cache:${b}:u1`]: "cb",
      [`chat_outbox:${b}:u1`]: "ob",
      [`chat_drafts:${b}:u1`]: "db",
      "chat_servers": "[]",
      "unrelated": "x",
    });
    removeServerData(st, a);
    const left = [...st.raw.keys()];
    expect(left).toEqual(
      expect.arrayContaining([
        authKey(b),
        `chat_cache:${b}:u1`,
        `chat_outbox:${b}:u1`,
        `chat_drafts:${b}:u1`,
        "chat_servers",
        "unrelated",
      ]),
    );
    expect(left.some((k) => k.includes(`:${a}:`) || k === authKey(a))).toBe(false);
  });
});

// ===== T5：旧版键一次性迁移 =====
describe("migrateLegacyStorage", () => {
  const CUR = "http://cur.example";
  const CUR_ENC = encServer(CUR);

  it("旧版全局凭据迁到当前服务器名下，旧键删除", () => {
    const st = memStorage({
      chat_access: "a1",
      chat_refresh: "r1",
      chat_user: JSON.stringify(user("u1")),
      chat_server_url: "http://cur.example/",
    });
    migrateLegacyStorage(st, CUR);
    expect(readAuth(st, CUR_ENC)).toEqual({
      access: "a1",
      refresh: "r1",
      user: user("u1"),
    });
    for (const k of ["chat_access", "chat_refresh", "chat_user", "chat_server_url"]) {
      expect(st.raw.has(k)).toBe(false);
    }
  });

  it("旧版 chat_cache/chat_outbox 按其内嵌服务器重编码，未发出的 outbox 不丢", () => {
    const payload = JSON.stringify([{ clientMsgId: "m1" }]);
    const st = memStorage({
      "chat_cache:http://a:28080:u1": "cache-a",
      "chat_outbox:http://a:28080:u1": payload,
      "chat_cache:https://b.example:u2": "cache-b",
    });
    migrateLegacyStorage(st, CUR);
    const aEnc = encServer("http://a:28080");
    expect(st.raw.get(`chat_cache:${aEnc}:u1`)).toBe("cache-a");
    expect(st.raw.get(`chat_outbox:${aEnc}:u1`)).toBe(payload);
    expect(st.raw.get(`chat_cache:${encServer("https://b.example")}:u2`)).toBe("cache-b");
    expect(st.raw.has("chat_cache:http://a:28080:u1")).toBe(false);
    expect(st.raw.has("chat_outbox:http://a:28080:u1")).toBe(false);
    expect(st.raw.has("chat_cache:https://b.example:u2")).toBe(false);
  });

  it("旧版 chat_drafts:<uid>（无服务器）迁到当前服务器名下", () => {
    const st = memStorage({ "chat_drafts:u1": '{"c":"hi"}' });
    migrateLegacyStorage(st, CUR);
    expect(st.raw.get(`chat_drafts:${CUR_ENC}:u1`)).toBe('{"c":"hi"}');
    expect(st.raw.has("chat_drafts:u1")).toBe(false);
  });

  it("已是新格式的键原样保留，迁移幂等", () => {
    const st = memStorage({
      [`chat_cache:${CUR_ENC}:u1`]: "c",
      [authKey(CUR_ENC)]: JSON.stringify({ access: "a", refresh: "r", user: user("u1") }),
    });
    migrateLegacyStorage(st, CUR);
    migrateLegacyStorage(st, CUR); // 第二次不再产生变化
    expect(st.raw.get(`chat_cache:${CUR_ENC}:u1`)).toBe("c");
    expect(readAuth(st, CUR_ENC)?.refresh).toBe("r");
  });

  it("发现的旧服务器进入服务器列表，当前服务器在最前", () => {
    const st = memStorage({
      chat_server_url: CUR,
      "chat_cache:http://old.example:u1": "x",
    });
    migrateLegacyStorage(st, CUR);
    const list = listServers(st);
    expect(list[0].url).toBe(CUR);
    expect(list.map((s) => s.url)).toContain("http://old.example");
  });

  it("凭据残缺（缺 refresh 或 user）时只删旧键不生成 auth", () => {
    const st = memStorage({ chat_access: "a1", chat_user: "{bad" });
    migrateLegacyStorage(st, CUR);
    expect(readAuth(st, CUR_ENC)).toBeNull();
    expect(st.raw.has("chat_access")).toBe(false);
    expect(st.raw.has("chat_user")).toBe(false);
  });
});

describe("kickReasonKey", () => {
  it("按服务器分键", () => {
    expect(kickReasonKey(encServer("http://a"))).toBe(
      `chat_kick_reason:${encServer("http://a")}`,
    );
    expect(kickReasonKey(encServer("http://a"))).not.toBe(
      kickReasonKey(encServer("http://b")),
    );
  });
});

// ===== T5：列表里同地址已登录用户名的记录（saveSession 走这里） =====
describe("upsertServer 记录最近登录用户名", () => {
  it("patch username/name 并顶到最前，未提供的字段保留", () => {
    const st = memStorage();
    upsertServer(st, "http://a", { username: "u1" });
    upsertServer(st, "http://a", { name: "测试服" });
    const e: ServerEntry = listServers(st)[0];
    expect(e.username).toBe("u1");
    expect(e.name).toBe("测试服");
  });
});
