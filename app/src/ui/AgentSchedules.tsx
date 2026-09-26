import { useEffect, useState } from "react";
import { agents, ApiError, convs } from "../api";
import { describeSchedule } from "../scheduleText";
import type { ConversationView, ScheduleSpec, ScheduleView } from "../types";

function errText(e: unknown): string {
  if (!(e instanceof ApiError)) return e instanceof Error ? e.message : "保存失败";
  const known: Record<string, string> = {
    bad_name: "名称需要 1–40 个字",
    bad_prompt: "指令需要 1–4000 个字",
    bad_spec: "时间表不合法",
    bad_timezone: "时区不合法",
    not_member: "你不在这个会话里",
    readonly_dm: "只读私聊不能作为目标",
    too_many: "最多 50 条定时任务",
  };
  if (e.status === 400) return known[e.message] ?? `参数不合法：${e.message}`;
  if (e.status === 404) return "找不到这条定时任务";
  return e.message || "保存失败";
}

function reasonText(reason: string): string {
  if (reason === "owner_disabled") return "已停用：账号已禁用";
  if (reason === "agent_deleted") return "已停用：Agent 已删除";
  if (reason === "not_member") return "已停用：你已不在目标会话";
  return `已停用：${reason}`;
}

function formatWhen(iso: string | null): string {
  if (!iso) return "无下次运行";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString();
}

function convLabel(c: ConversationView, agentConvId: string): string {
  if (c.id === agentConvId) return "专属会话";
  if (c.kind === "group") return c.name || "群聊";
  if (c.kind === "dm") return c.peer?.display_name || c.peer?.username || "私聊";
  if (c.kind === "self") return "文件传输助手";
  if (c.kind === "agent") return c.agent?.name ? `${c.agent.name} 的专属会话` : "专属会话";
  return c.name || "会话";
}

function browserZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  } catch {
    return "UTC";
  }
}

const WEEK = [
  [1, "一"],
  [2, "二"],
  [3, "三"],
  [4, "四"],
  [5, "五"],
  [6, "六"],
  [7, "日"],
] as const;

/** Agent 编辑器里的定时任务：列表、开关、删除、新建。 */
export default function AgentSchedules({
  agentId,
  conversationId,
}: {
  agentId: string;
  conversationId: string;
}) {
  const [rows, setRows] = useState<ScheduleView[]>([]);
  const [targets, setTargets] = useState<{ id: string; label: string }[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [busyId, setBusyId] = useState("");

  const [name, setName] = useState("");
  const [prompt, setPrompt] = useState("");
  const [target, setTarget] = useState(conversationId);
  const [kind, setKind] = useState<"daily" | "weekly" | "once" | "interval">("daily");
  const [time, setTime] = useState("09:00");
  const [days, setDays] = useState<number[]>([1, 3, 5]);
  const [minutes, setMinutes] = useState("30");
  const [onceLocal, setOnceLocal] = useState("");
  const [timezone, setTimezone] = useState(browserZone);

  const reload = () =>
    agents
      .schedules(agentId)
      .then(setRows)
      .catch((e) => setErr(errText(e)));

  useEffect(() => {
    let alive = true;
    Promise.all([agents.schedules(agentId), convs.list()])
      .then(([list, conversations]) => {
        if (!alive) return;
        setRows(list);
        const opts = conversations.map((c) => ({ id: c.id, label: convLabel(c, conversationId) }));
        if (!opts.some((o) => o.id === conversationId)) {
          opts.unshift({ id: conversationId, label: "专属会话" });
        }
        setTargets(opts);
        setLoaded(true);
      })
      .catch((e) => {
        if (!alive) return;
        setErr(errText(e));
        setLoaded(true);
      });
    return () => {
      alive = false;
    };
  }, [agentId, conversationId]);

  const buildSpec = (): ScheduleSpec | string => {
    const hhmm = time.slice(0, 5);
    if (kind === "daily") return { type: "daily", time: hhmm };
    if (kind === "weekly") {
      if (days.length === 0) return "请至少选一个星期";
      return { type: "weekly", days: [...days].sort((a, b) => a - b), time: hhmm };
    }
    if (kind === "interval") {
      const n = Number(minutes);
      if (!Number.isInteger(n) || n < 5) return "间隔至少 5 分钟";
      return { type: "interval", minutes: n };
    }
    if (!onceLocal) return "请填写运行时间";
    const at = new Date(onceLocal).toISOString();
    if (Number.isNaN(Date.parse(at))) return "运行时间不合法";
    return { type: "once", at };
  };

  const create = async () => {
    if (busy) return;
    const spec = buildSpec();
    if (typeof spec === "string") {
      setErr(spec);
      return;
    }
    const trimmed = name.trim();
    if (!trimmed || !prompt.trim()) {
      setErr("请填写名称和指令");
      return;
    }
    setBusy(true);
    setErr("");
    try {
      const created = await agents.createSchedule(agentId, {
        name: trimmed,
        prompt: prompt.trim(),
        target_conversation_id: target,
        spec,
        timezone: timezone.trim() || "UTC",
      });
      setRows((cur) => [...cur, created]);
      setName("");
      setPrompt("");
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(false);
    }
  };

  const toggle = async (row: ScheduleView, enabled: boolean) => {
    setBusyId(row.id);
    setErr("");
    try {
      const updated = await agents.patchSchedule(row.id, { enabled });
      setRows((cur) => cur.map((x) => (x.id === row.id ? updated : x)));
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusyId("");
    }
  };

  const remove = async (row: ScheduleView) => {
    setBusyId(row.id);
    setErr("");
    try {
      await agents.deleteSchedule(row.id);
      setRows((cur) => cur.filter((x) => x.id !== row.id));
    } catch (e) {
      setErr(errText(e));
      void reload();
    } finally {
      setBusyId("");
    }
  };

  return (
    <div className="agent-schedules">
      <h4 className="agent-sec">定时任务</h4>
      {!loaded ? (
        <div className="muted">加载中…</div>
      ) : rows.length === 0 ? (
        <div className="muted">还没有定时任务</div>
      ) : (
        <div className="sched-list">
          {rows.map((row) => (
            <div className="sched-row" key={row.id}>
              <div className="sched-main">
                <div className="sched-name">{row.name}</div>
                <div className="sched-meta">{describeSchedule(row.spec, row.timezone)}</div>
                <div className="sched-meta">下次运行 {formatWhen(row.next_run_at)}</div>
                {row.disabled_reason && <div className="sched-meta">{reasonText(row.disabled_reason)}</div>}
              </div>
              <label className="sched-enable">
                <input
                  type="checkbox"
                  checked={row.enabled}
                  aria-label={`启用 ${row.name}`}
                  disabled={busyId === row.id}
                  onChange={(e) => void toggle(row, e.target.checked)}
                />
                启用
              </label>
              <button
                type="button"
                className="danger"
                aria-label={`删除定时任务 ${row.name}`}
                disabled={busyId === row.id}
                onClick={() => void remove(row)}
              >
                删除
              </button>
            </div>
          ))}
        </div>
      )}

      <div className="sched-form">
        <input
          aria-label="定时任务名称"
          placeholder="名称（1–40 字）"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
        <textarea
          aria-label="定时任务指令"
          placeholder="到点执行的指令"
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
          rows={2}
        />
        <label className="agent-field">
          <span className="muted">目标会话</span>
          <select aria-label="目标会话" value={target} onChange={(e) => setTarget(e.target.value)}>
            {targets.map((t) => (
              <option key={t.id} value={t.id}>
                {t.label}
              </option>
            ))}
          </select>
        </label>
        <label className="agent-field">
          <span className="muted">时间表</span>
          <select
            aria-label="时间表类型"
            value={kind}
            onChange={(e) => setKind(e.target.value as typeof kind)}
          >
            <option value="daily">每天</option>
            <option value="weekly">每周</option>
            <option value="once">一次</option>
            <option value="interval">每隔</option>
          </select>
        </label>
        {kind === "weekly" && (
          <div className="sched-days">
            {WEEK.map(([n, label]) => (
              <label key={n}>
                <input
                  type="checkbox"
                  checked={days.includes(n)}
                  onChange={(e) =>
                    setDays((cur) => (e.target.checked ? [...cur, n] : cur.filter((d) => d !== n)))
                  }
                />
                周{label}
              </label>
            ))}
          </div>
        )}
        {(kind === "daily" || kind === "weekly") && (
          <label className="agent-field">
            <span className="muted">时刻</span>
            <input aria-label="定时时刻" type="time" value={time} onChange={(e) => setTime(e.target.value)} />
          </label>
        )}
        {kind === "once" && (
          <label className="agent-field">
            <span className="muted">运行时间</span>
            <input
              aria-label="运行时间"
              type="datetime-local"
              value={onceLocal}
              onChange={(e) => setOnceLocal(e.target.value)}
            />
          </label>
        )}
        {kind === "interval" && (
          <label className="agent-field">
            <span className="muted">间隔（分钟，至少 5）</span>
            <input
              aria-label="间隔分钟"
              type="number"
              min={5}
              value={minutes}
              onChange={(e) => setMinutes(e.target.value)}
            />
          </label>
        )}
        <label className="agent-field">
          <span className="muted">时区</span>
          <input aria-label="时区" value={timezone} onChange={(e) => setTimezone(e.target.value)} />
        </label>
        {err && <div className="auth-err">{err}</div>}
        <button type="button" className="primary" disabled={busy} onClick={() => void create()}>
          {busy ? "添加中…" : "添加定时任务"}
        </button>
      </div>
    </div>
  );
}
