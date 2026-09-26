// Agent A17：时间表的纯计算。
// 「spec + 时区 + 当前时间 → 下次运行时间」，结果严格晚于 now。
// 错过的周期不回填：调用方用 now() 来算下一次，而不是从上次槽位往前补。
//
// 夏令时：
// - 春天拨快产生的缺口（该钟点不存在）跳过这一天，取之后第一个存在的同一钟点；
// - 秋天拨回产生的重叠先取较早的一次，若它已经不晚于 now，再取较晚的一次。

use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveTime, SecondsFormat, TimeZone, Timelike, Utc,
};
use chrono_tz::Tz;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSpec {
    Once {
        at: DateTime<Utc>,
    },
    Daily {
        time: NaiveTime,
    },
    /// days：1 = 周一 … 7 = 周日，已去重并升序
    Weekly {
        days: Vec<u8>,
        time: NaiveTime,
    },
    Interval {
        minutes: i64,
    },
}

impl ScheduleSpec {
    pub fn is_once(&self) -> bool {
        matches!(self, Self::Once { .. })
    }
}

/// 定时触发前的准入。`Some(reason)` → 停用该条目、不建调用。
///
/// A08 挂点：合并「使用权」时只改这个函数。全局关闭或该用户被禁止时返回
/// `Some("agent_access")`；管理员始终返回 `None`。扫描侧 `claim_one` 不用改流程。
pub fn schedule_block_reason(
    owner_disabled: bool,
    agent_deleted: bool,
    still_member: bool,
) -> Option<&'static str> {
    if owner_disabled {
        Some("owner_disabled")
    } else if agent_deleted {
        Some("agent_deleted")
    } else if !still_member {
        Some("not_member")
    } else {
        None
    }
}

pub fn parse_timezone(name: &str) -> Result<Tz, &'static str> {
    if name.is_empty() || name.len() > 64 {
        return Err("bad_timezone");
    }
    name.parse::<Tz>().map_err(|_| "bad_timezone")
}

pub fn parse_spec(v: &Value) -> Result<ScheduleSpec, &'static str> {
    let kind = v.get("type").and_then(Value::as_str).ok_or("bad_spec")?;
    match kind {
        "once" => {
            let at = v.get("at").and_then(Value::as_str).ok_or("bad_spec")?;
            let at = DateTime::parse_from_rfc3339(at)
                .map_err(|_| "bad_spec")?
                .with_timezone(&Utc);
            Ok(ScheduleSpec::Once { at })
        }
        "daily" => Ok(ScheduleSpec::Daily {
            time: parse_hhmm(v.get("time").and_then(Value::as_str).ok_or("bad_spec")?)?,
        }),
        "weekly" => {
            let time = parse_hhmm(v.get("time").and_then(Value::as_str).ok_or("bad_spec")?)?;
            let raw = v.get("days").and_then(Value::as_array).ok_or("bad_spec")?;
            if raw.is_empty() || raw.len() > 7 {
                return Err("bad_spec");
            }
            let mut days = Vec::with_capacity(raw.len());
            for d in raw {
                let n = d.as_i64().ok_or("bad_spec")?;
                if !(1..=7).contains(&n) {
                    return Err("bad_spec");
                }
                let n = n as u8;
                if days.contains(&n) {
                    return Err("bad_spec");
                }
                days.push(n);
            }
            days.sort_unstable();
            Ok(ScheduleSpec::Weekly { days, time })
        }
        "interval" => {
            let n = v.get("minutes").and_then(Value::as_i64).ok_or("bad_spec")?;
            // 任务只规定 N ≥ 5。上限挡住 Duration 溢出，十年足够。
            if !(5..=5_256_000).contains(&n) {
                return Err("bad_spec");
            }
            Ok(ScheduleSpec::Interval { minutes: n })
        }
        _ => Err("bad_spec"),
    }
}

pub fn spec_to_json(spec: &ScheduleSpec) -> Value {
    match spec {
        ScheduleSpec::Once { at } => json!({
            "type": "once",
            "at": at.to_rfc3339_opts(SecondsFormat::Secs, true),
        }),
        ScheduleSpec::Daily { time } => json!({
            "type": "daily",
            "time": format_hhmm(*time),
        }),
        ScheduleSpec::Weekly { days, time } => json!({
            "type": "weekly",
            "days": days,
            "time": format_hhmm(*time),
        }),
        ScheduleSpec::Interval { minutes } => json!({
            "type": "interval",
            "minutes": minutes,
        }),
    }
}

/// 下一次运行时刻，严格晚于 `now`。`once` 若 `at` 已经不在未来则返回 None。
pub fn next_run(spec: &ScheduleSpec, tz: Tz, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match spec {
        ScheduleSpec::Once { at } => (*at > now).then_some(*at),
        ScheduleSpec::Daily { time } => next_clock(tz, None, *time, now),
        ScheduleSpec::Weekly { days, time } => next_clock(tz, Some(days), *time, now),
        ScheduleSpec::Interval { minutes } => {
            let _ = tz;
            Some(now + Duration::minutes(*minutes))
        }
    }
}

fn parse_hhmm(s: &str) -> Result<NaiveTime, &'static str> {
    let (h, m) = s.split_once(':').ok_or("bad_spec")?;
    if h.len() != 2
        || m.len() != 2
        || !h.chars().all(|c| c.is_ascii_digit())
        || !m.chars().all(|c| c.is_ascii_digit())
    {
        return Err("bad_spec");
    }
    let h: u32 = h.parse().map_err(|_| "bad_spec")?;
    let m: u32 = m.parse().map_err(|_| "bad_spec")?;
    NaiveTime::from_hms_opt(h, m, 0).ok_or("bad_spec")
}

fn format_hhmm(t: NaiveTime) -> String {
    format!("{:02}:{:02}", t.hour(), t.minute())
}

fn instants_at(tz: Tz, date: NaiveDate, time: NaiveTime) -> Vec<DateTime<Utc>> {
    match tz.with_ymd_and_hms(
        date.year(),
        date.month(),
        date.day(),
        time.hour(),
        time.minute(),
        time.second(),
    ) {
        chrono::LocalResult::None => Vec::new(),
        chrono::LocalResult::Single(dt) => vec![dt.with_timezone(&Utc)],
        chrono::LocalResult::Ambiguous(early, late) => {
            let mut v = vec![early.with_timezone(&Utc), late.with_timezone(&Utc)];
            v.sort();
            v.dedup();
            v
        }
    }
}

fn next_clock(
    tz: Tz,
    days: Option<&[u8]>,
    time: NaiveTime,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let start = now.with_timezone(&tz).date_naive();
    for offset in 0..14 {
        let date = start.checked_add_signed(Duration::days(offset))?;
        if let Some(days) = days {
            let wd = date.weekday().number_from_monday() as u8;
            if !days.contains(&wd) {
                continue;
            }
        }
        for cand in instants_at(tz, date, time) {
            if cand > now {
                return Some(cand);
            }
        }
    }
    None
}
