/** 定时任务的时间表。与服务端 spec jsonb 同形。 */
export interface ScheduleSpec {
  type: "once" | "daily" | "weekly" | "interval" | string;
  at?: string;
  time?: string;
  days?: number[];
  minutes?: number;
}

const WEEKDAY = ["", "一", "二", "三", "四", "五", "六", "日"];

function clockInZone(at: string, timeZone: string): string {
  const date = new Date(at);
  if (Number.isNaN(date.getTime())) return at;
  let zone = timeZone || "UTC";
  try {
    Intl.DateTimeFormat(undefined, { timeZone: zone });
  } catch {
    zone = "UTC";
  }
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone: zone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
  }).formatToParts(date);
  const get = (t: string) => parts.find((p) => p.type === t)?.value ?? "";
  let hour = get("hour");
  if (hour === "24") hour = "00";
  return `${get("year")}-${get("month")}-${get("day")} ${hour}:${get("minute")}`;
}

/** 时间表的中文描述。once 按 timezone 显示钟点（缺省 UTC）。 */
export function describeSchedule(spec: ScheduleSpec, timezone = "UTC"): string {
  if (spec.type === "daily" && spec.time) return `每天 ${spec.time}`;
  if (spec.type === "weekly" && spec.time) {
    const days = [...(spec.days ?? [])]
      .filter((d) => d >= 1 && d <= 7)
      .sort((a, b) => a - b)
      .map((d) => WEEKDAY[d]);
    return `每周${days.join("、")} ${spec.time}`;
  }
  if (spec.type === "interval" && typeof spec.minutes === "number") {
    return `每 ${spec.minutes} 分钟`;
  }
  if (spec.type === "once" && spec.at) {
    return `一次，${clockInZone(spec.at, timezone)}`;
  }
  return "时间表";
}
