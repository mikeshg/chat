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

/** Agent 编辑器里的定时任务：列表、开关、删除、新建。改动立即生效。 */
export default function AgentSchedules({
  agentId,
  conversationId,
  onCount,
}: {
  agentId: string;
  conversationId: string;
  onCount?: (n: number) => void;
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
    onCount?.(rows.length);
  }, [rows.length, onCount]);

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
    <div className="am-sched-wrap">
      <div className="am-field">
        <span className="am-lb">定时任务</span>
        {!loaded ? (
          <div className="am-hint">加载中…</div>
        ) : rows.length === 0 ? (
          <div className="am-hint">还没有定时任务。</div>
        ) : (
          <div className="am-sched">
            {rows.map((row) => {
              const targetLabel = targets.find((t) => t.id === row.target_conversation_id)?.label ?? "会话";
              return (
                <div className="am-sched-row" key={row.id}>
                  <div className="grow">
                    <b>{row.name}</b>
                    <span>
                      {describeSchedule(row.spec, row.timezone)} · 发到{targetLabel}
                      {row.next_run_at ? ` · 下次 ${formatWhen(row.next_run_at)}` : ""}
                    </span>
                    {row.disabled_reason ? <span>{reasonText(row.disabled_reason)}</span> : null}
                  </div>
                  <button
                    type="button"
                    className="am-switch"
                    role="switch"
                    aria-checked={row.enabled}
                    aria-label={`启用 ${row.name}`}
                    disabled={busyId === row.id}
                    onClick={() => void toggle(row, !row.enabled)}
                  />
                  <button
                    type="button"
                    className="am-ghost"
                    aria-label={`删除 ${row.name}`}
                    disabled={busyId === row.id}
                    onClick={() => void remove(row)}
                  >
                    ✕
                  </button>
                </div>
              );
            })}
          </div>
        )}
      </div>

      <details className="am-adv" open>
        <summary>新建定时任务</summary>
        <div className="am-adv-in">
          <input
            aria-label="名称"
            placeholder="名称，例如：每日站会摘要"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <textarea
            aria-label="到点执行的指令"
            placeholder="到点让它做什么，例如：总结昨天合并的提交"
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            rows={3}
          />
          <select aria-label="发到哪个会话" value={target} onChange={(e) => setTarget(e.target.value)}>
            {targets.map((t) => (
              <option key={t.id} value={t.id}>
                发到{t.label}
              </option>
            ))}
          </select>
          <div className="am-row">
            <select aria-label="重复" value={kind} onChange={(e) => setKind(e.target.value as typeof kind)}>
              <option value="daily">每天</option>
              <option value="weekly">每周</option>
              <option value="once">一次</option>
              <option value="interval">每隔</option>
            </select>
            {(kind === "daily" || kind === "weekly") && (
              <input aria-label="时刻" type="time" value={time} onChange={(e) => setTime(e.target.value)} />
            )}
          </div>
          {kind === "weekly" && (
            <div className="am-days">
              {WEEK.map(([n, label]) => (
                <button
                  key={n}
                  type="button"
                  aria-pressed={days.includes(n)}
                  onClick={() => setDays((cur) => (cur.includes(n) ? cur.filter((d) => d !== n) : [...cur, n]))}
                >
                  周{label}
                </button>
              ))}
            </div>
          )}
          {kind === "once" && (
            <input
              aria-label="运行时间"
              type="datetime-local"
              value={onceLocal}
              onChange={(e) => setOnceLocal(e.target.value)}
            />
          )}
          {kind === "interval" && (
            <input
              aria-label="间隔分钟"
              type="number"
              min={5}
              value={minutes}
              onChange={(e) => setMinutes(e.target.value)}
            />
          )}
          <input aria-label="时区" value={timezone} onChange={(e) => setTimezone(e.target.value)} />
          {err && <div className="auth-err">{err}</div>}
          <div>
            <button type="button" className="primary" disabled={busy} onClick={() => void create()}>
              {busy ? "添加中…" : "添加"}
            </button>
          </div>
        </div>
      </details>
    </div>
  );
}
