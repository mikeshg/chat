import { describe, expect, it } from "vitest";
import type { AgentView } from "../types";
import {
  ADD_ON_DESKTOP_HINT,
  NO_AGENTS_ON_COMPUTER,
  agentDraftDirty,
  agentPlaceLine,
  agentRowMeta,
  agentSaveBar,
  canAddAgent,
  draftFromAgent,
  filterPrograms,
  groupOptions,
  followDefaultOutputHint,
  followDefaultQueueOption,
  hostGroupStatus,
  outputModeLabel,
  queueTimeoutLabel,
  readInjectedLocalHostId,
  resolveLocalComputer,
  showLocalTag,
} from "./agentPage";

function agent(partial: Partial<AgentView> = {}): AgentView {
  return {
    id: "a1",
    name: "coder",
    host_id: "h1",
    host_online: true,
    runtime: { command: "node", args: ["agent.js"] },
    cwd: "D:\\work",
    conversation_id: "c1",
    created_at: "2026-01-01T00:00:00Z",
    extra_dirs: [],
    model: "",
    mode: "",
    instructions: "",
    env: {},
    output_mode: null,
    queue_timeout_secs: null,
    permission_tier: "read_only",
    ...partial,
  };
}

describe("Agent A26：列表分组状态", () => {
  const hosts = [
    { id: "local", label: "14X", online: true },
    { id: "off", label: "家里", online: false },
  ];

  it("离线才显示最后在线；在线不带最后在线", () => {
    expect(hostGroupStatus("windows", true, "9/25 22:40")).toBe("Windows · 在线");
    expect(hostGroupStatus("macos", false, "9/25 22:40")).toBe("macOS · 离线，最后在线 9/25 22:40");
    expect(hostGroupStatus("", false, "")).toBe("离线");
    expect(NO_AGENTS_ON_COMPUTER).toBe("这台电脑上还没有 Agent");
    expect(ADD_ON_DESKTOP_HINT).toContain("桌面端");
  });

  it("本机标签和添加按钮只在桌面端、且本机在线时出现", () => {
    const desktop = resolveLocalComputer({
      desktopShell: true,
      storeHostId: "local",
      injectedHostId: null,
    });
    expect(showLocalTag(desktop, "local")).toBe(true);
    expect(showLocalTag(desktop, "off")).toBe(false);
    expect(canAddAgent(desktop, hosts)).toBe(true);
    expect(canAddAgent(desktop, [{ id: "local", online: false }])).toBe(false);

    const phone = resolveLocalComputer({
      desktopShell: false,
      storeHostId: null,
      injectedHostId: null,
    });
    expect(phone).toEqual({ desktop: false, localHostId: null });
    expect(showLocalTag(phone, "local")).toBe(false);
    expect(canAddAgent(phone, hosts)).toBe(false);

    const waiting = resolveLocalComputer({
      desktopShell: true,
      storeHostId: null,
      injectedHostId: null,
    });
    expect(canAddAgent(waiting, hosts)).toBe(false);
  });

  it("测试页注入的 host id 视为本机，网页上的 store id 不算", () => {
    expect(
      resolveLocalComputer({
        desktopShell: false,
        storeHostId: "ignored",
        injectedHostId: " host-1 ",
      }),
    ).toEqual({ desktop: true, localHostId: "host-1" });
  });

  it("列表副标题是程序名和权限档位", () => {
    expect(agentRowMeta({ registry: { id: "claude", version: "1" } }, "workspace", "Claude Code")).toBe(
      "Claude Code · 可改工作目录",
    );
    expect(agentRowMeta({ command: "node", args: [] }, "read_only")).toBe("自定义命令 · node · 只读");
  });

  it("详情头部：本机在线 / 离线排队", () => {
    expect(agentPlaceLine("14X", true, true)).toBe("运行在 14X（本机） · 在线");
    expect(agentPlaceLine("家里", false, false)).toBe("运行在 家里 · 离线，新调用会排队");
  });
});

describe("Agent A26：搜索过滤", () => {
  const entries = [
    { id: "claude", name: "Claude Code" },
    { id: "codex", name: "Codex" },
    { id: "gemini", name: "Gemini CLI" },
  ];

  it("按名字或 id 过滤，空搜索返回全部", () => {
    expect(filterPrograms(entries, "").map((e) => e.id)).toEqual(["claude", "codex", "gemini"]);
    expect(filterPrograms(entries, " CLAU ").map((e) => e.id)).toEqual(["claude"]);
    expect(filterPrograms(entries, "codex").map((e) => e.id)).toEqual(["codex"]);
    expect(filterPrograms(entries, "没有")).toEqual([]);
  });
});

describe("Agent A26：草稿与保存条", () => {
  it("没改动不显示保存条，任一字段改动后出现", () => {
    const saved = draftFromAgent(agent());
    expect(agentDraftDirty(saved, saved)).toBe(false);
    expect(agentSaveBar(false)).toEqual({ visible: false, text: "" });
    expect(agentSaveBar(true)).toEqual({ visible: true, text: "有未保存的修改" });
    expect(agentDraftDirty(saved, { ...saved, name: "reviewer" })).toBe(true);
    expect(agentDraftDirty(saved, { ...saved, extraDirs: ["D:\\lib"] })).toBe(true);
    expect(agentDraftDirty(saved, { ...saved, envRows: [{ key: "A", value: "1" }] })).toBe(true);
    expect(agentDraftDirty(saved, { ...saved, outputMode: "status" })).toBe(true);
    expect(agentDraftDirty(saved, { ...saved, queueTimeout: "0" })).toBe(true);
    expect(agentDraftDirty(saved, { ...saved, permissionTier: "full" })).toBe(true);
  });

  it("跟随默认是空串，显式 0 秒（永不）算改动", () => {
    const saved = draftFromAgent(agent({ queue_timeout_secs: null, output_mode: null }));
    expect(saved.outputMode).toBe("");
    expect(saved.queueTimeout).toBe("");
    const never = draftFromAgent(agent({ queue_timeout_secs: 0 }));
    expect(never.queueTimeout).toBe("0");
    expect(agentDraftDirty(saved, never)).toBe(true);
  });
});

describe("Agent A26：跟随默认文案", () => {
  it("写明当前默认值", () => {
    expect(outputModeLabel("stream")).toBe("实时流式");
    expect(queueTimeoutLabel(86400)).toBe("1 天");
    expect(followDefaultOutputHint("实时流式")).toBe("现在是「实时流式」");
    expect(followDefaultQueueOption("1 天")).toBe("跟随默认设置（现在是 1 天）");
    expect(followDefaultOutputHint(outputModeLabel("status"))).toBe("现在是「状态 + 最终结果」");
    expect(followDefaultQueueOption(queueTimeoutLabel(0))).toBe("跟随默认设置（现在是 永不）");
  });
});

describe("readInjectedLocalHostId", () => {
  it("只接受非空字符串", () => {
    const g = globalThis as { __CHAT_LOCAL_HOST__?: unknown };
    const prev = g.__CHAT_LOCAL_HOST__;
    g.__CHAT_LOCAL_HOST__ = "  abc  ";
    expect(readInjectedLocalHostId()).toBe("abc");
    g.__CHAT_LOCAL_HOST__ = "";
    expect(readInjectedLocalHostId()).toBeNull();
    g.__CHAT_LOCAL_HOST__ = prev;
  });
});

// ===== Agent A27 =====
import type { AgentHostView, ConversationView } from "../types";
import { folderName, optionName, pickHost, sessionName, workspaceSessions } from "./agentPage";

describe("A27 工作区与对话", () => {
  const host = (id: string, online: boolean): AgentHostView => ({
    id,
    label: id,
    platform: "windows",
    online,
    last_seen_at: "2026-09-29T00:00:00Z",
  });
  const conv = (id: string, ws: string | null, at: string): ConversationView =>
    ({
      id,
      kind: "agent",
      name: null,
      owner_id: "u",
      peer: null,
      member_count: 1,
      last_seq: 0,
      unread_count: 0,
      has_mention: false,
      last_message: null,
      agent: { id: "a", name: "bot", host_id: "h", host_online: true, workspace_id: ws },
      created_at: at,
    }) as ConversationView;

  it("工作区名取路径最后一段，Windows / Unix 都认", () => {
    expect(folderName("D:\\projects\\chat\\")).toBe("chat");
    expect(folderName("/home/me/app")).toBe("app");
    expect(folderName("C:")).toBe("C:");
    expect(folderName("")).toBe("无工作区");
  });

  it("选电脑：保持当前 → 本机 → 在线 → 第一台", () => {
    const hs = [host("a", false), host("b", true), host("c", true)];
    expect(pickHost(hs, "c", "a")).toBe("a");
    expect(pickHost(hs, "c", "gone")).toBe("c");
    expect(pickHost(hs, null, null)).toBe("b");
    expect(pickHost([host("a", false)], null, null)).toBe("a");
    expect(pickHost([], null, null)).toBeNull();
  });

  it("工作区对话只取本工作区的，最近的在前；专属会话不算", () => {
    const list = [
      conv("old", "w1", "2026-09-01T00:00:00Z"),
      conv("home", null, "2026-09-30T00:00:00Z"),
      conv("new", "w1", "2026-09-20T00:00:00Z"),
      conv("other", "w2", "2026-09-25T00:00:00Z"),
    ];
    expect(workspaceSessions(list, "w1").map((c) => c.id)).toEqual(["new", "old"]);
  });

  it("对话名取第一行、最多 30 字；可选项名找不到时显示 id", () => {
    expect(sessionName("  修一下测试\n细节如下")).toBe("修一下测试");
    expect([...sessionName("字".repeat(40))].length).toBe(30);
    expect(optionName("smart", [{ id: "smart", name: "Smart" }])).toBe("Smart");
    expect(optionName("x", undefined)).toBe("x");
  });
});

describe("groupOptions", () => {
  it("按「供应商 · 」分组，组内只留模型名", () => {
    expect(
      groupOptions([
        { id: "a", name: "sub · grok-4.7" },
        { id: "b", name: "api · gpt-5" },
        { id: "c", name: "sub · gpt-5" },
        { id: "d", name: "Smart" },
      ]),
    ).toEqual({
      loose: [{ id: "d", name: "Smart" }],
      groups: [
        ["sub", [{ id: "a", name: "grok-4.7" }, { id: "c", name: "gpt-5" }]],
        ["api", [{ id: "b", name: "gpt-5" }]],
      ],
    });
  });
});
