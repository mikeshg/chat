//! A17：时间表纯函数。期望时刻是手算的钟点，不调用被测函数反推。

use chat_server::schedules::spec;
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::json;

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn tz(name: &str) -> Tz {
    name.parse().unwrap()
}

fn daily(time: &str) -> spec::ScheduleSpec {
    spec::parse_spec(&json!({"type":"daily","time":time})).unwrap()
}

fn weekly(days: &[i64], time: &str) -> spec::ScheduleSpec {
    spec::parse_spec(&json!({"type":"weekly","days":days,"time":time})).unwrap()
}

#[test]
fn daily_shanghai_same_day_and_next_day() {
    let shanghai = tz("Asia/Shanghai");
    let spec = daily("09:00");
    // 2026-01-15 08:00 CST = 00:00 UTC，当天 09:00 还没到
    let morning = shanghai
        .with_ymd_and_hms(2026, 1, 15, 8, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    let today = shanghai
        .with_ymd_and_hms(2026, 1, 15, 9, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(spec::next_run(&spec, shanghai, morning), Some(today));
    // 10:00 CST 已过 09:00 → 明天同一钟点
    let late = shanghai
        .with_ymd_and_hms(2026, 1, 15, 10, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    let tomorrow = shanghai
        .with_ymd_and_hms(2026, 1, 16, 9, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(spec::next_run(&spec, shanghai, late), Some(tomorrow));
    // 刚好踩在 09:00:00 上，严格晚于 now，所以是明天
    let exact = shanghai
        .with_ymd_and_hms(2026, 1, 15, 9, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(spec::next_run(&spec, shanghai, exact), Some(tomorrow));
}

#[test]
fn weekly_crosses_the_week_boundary() {
    let utc_tz = tz("UTC");
    // 2026-03-15 是周日。周一、三、五 09:00 → 下一次是 3/16 周一
    let sunday = utc("2026-03-15T10:00:00Z");
    assert_eq!(
        sunday.with_timezone(&utc_tz).weekday().number_from_monday(),
        7
    );
    let spec = weekly(&[1, 3, 5], "09:00");
    let monday = utc("2026-03-16T09:00:00Z");
    assert_eq!(spec::next_run(&spec, utc_tz, sunday), Some(monday));
    // 周五 10:00 已过当天 09:00 → 下周一，而不是本周再补一次
    let friday = utc("2026-03-20T10:00:00Z");
    assert_eq!(
        friday.with_timezone(&utc_tz).weekday().number_from_monday(),
        5
    );
    let next_monday = utc("2026-03-23T09:00:00Z");
    assert_eq!(spec::next_run(&spec, utc_tz, friday), Some(next_monday));
    // 周五 08:00 还没到 09:00 → 就是今天
    let friday_morning = utc("2026-03-20T08:00:00Z");
    assert_eq!(
        spec::next_run(&spec, utc_tz, friday_morning),
        Some(utc("2026-03-20T09:00:00Z"))
    );
}

#[test]
fn interval_is_now_plus_n_not_a_backfill() {
    let now = utc("2026-05-01T00:00:00Z");
    let spec = spec::parse_spec(&json!({"type":"interval","minutes":5})).unwrap();
    let utc_tz = tz("UTC");
    let shanghai = tz("Asia/Shanghai");
    assert_eq!(
        spec::next_run(&spec, utc_tz, now),
        Some(now + Duration::minutes(5))
    );
    // 时区不改变间隔
    assert_eq!(
        spec::next_run(&spec, shanghai, now),
        Some(now + Duration::minutes(5))
    );
}

#[test]
fn once_future_and_past() {
    let now = utc("2026-06-01T12:00:00Z");
    let future = spec::parse_spec(&json!({
        "type": "once",
        "at": "2026-06-01T15:00:00Z"
    }))
    .unwrap();
    assert_eq!(
        spec::next_run(&future, tz("UTC"), now),
        Some(utc("2026-06-01T15:00:00Z"))
    );
    let past = spec::parse_spec(&json!({
        "type": "once",
        "at": "2026-06-01T11:00:00Z"
    }))
    .unwrap();
    assert_eq!(spec::next_run(&past, tz("UTC"), now), None);
}

#[test]
fn dst_spring_forward_skips_missing_clock_time() {
    // 2026-03-08 美国东部 02:00 → 03:00，02:30 当天不存在。
    let ny = tz("America/New_York");
    let spec = daily("02:30");
    let now = ny
        .with_ymd_and_hms(2026, 3, 8, 1, 0, 0)
        .single()
        .unwrap()
        .with_timezone(&Utc);
    let got = spec::next_run(&spec, ny, now).unwrap();
    let local = got.with_timezone(&ny);
    assert_eq!(
        (
            local.year(),
            local.month(),
            local.day(),
            local.hour(),
            local.minute()
        ),
        (2026, 3, 9, 2, 30),
        "缺口日应跳到 3/9 02:30，实际 {local}"
    );
}

#[test]
fn dst_fall_back_picks_earlier_then_later() {
    // 2026-11-01 美国东部 02:00 拨回 01:00，01:30 出现两次。
    // 01:30 EDT = 05:30Z，01:30 EST = 06:30Z。
    let ny = tz("America/New_York");
    let spec = daily("01:30");
    let before_both = utc("2026-11-01T05:00:00Z");
    let earlier = ny
        .with_ymd_and_hms(2026, 11, 1, 1, 30, 0)
        .earliest()
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(spec::next_run(&spec, ny, before_both), Some(earlier));
    let between = utc("2026-11-01T06:00:00Z");
    let later = ny
        .with_ymd_and_hms(2026, 11, 1, 1, 30, 0)
        .latest()
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(spec::next_run(&spec, ny, between), Some(later));
    let after_both = utc("2026-11-01T07:00:00Z");
    let next_day = ny
        .with_ymd_and_hms(2026, 11, 2, 1, 30, 0)
        .single()
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(spec::next_run(&spec, ny, after_both), Some(next_day));
}

#[test]
fn parse_rejects_bad_shapes_and_canonicalizes_weekdays() {
    use spec::{parse_spec, parse_timezone, spec_to_json};
    assert!(parse_spec(&json!({"type":"interval","minutes":4})).is_err());
    assert!(parse_spec(&json!({"type":"daily","time":"9:00"})).is_err());
    assert!(parse_spec(&json!({"type":"daily","time":"24:00"})).is_err());
    assert!(parse_spec(&json!({"type":"weekly","days":[],"time":"09:00"})).is_err());
    assert!(parse_spec(&json!({"type":"weekly","days":[1,1],"time":"09:00"})).is_err());
    assert!(parse_spec(&json!({"type":"weekly","days":[0],"time":"09:00"})).is_err());
    assert!(parse_spec(&json!({"type":"cron","expr":"* * * * *"})).is_err());
    assert!(parse_timezone("Not/AZone").is_err());
    assert!(parse_timezone("").is_err());
    assert!(parse_timezone("Asia/Shanghai").is_ok());

    let spec = parse_spec(&json!({"type":"weekly","days":[5,1,3],"time":"09:00"})).unwrap();
    assert_eq!(spec_to_json(&spec)["days"], json!([1, 3, 5]));
}

#[test]
fn block_reason_priority() {
    use spec::schedule_block_reason;
    assert_eq!(schedule_block_reason(false, false, true), None);
    assert_eq!(
        schedule_block_reason(true, true, false),
        Some("owner_disabled")
    );
    assert_eq!(
        schedule_block_reason(false, true, true),
        Some("agent_deleted")
    );
    assert_eq!(
        schedule_block_reason(false, false, false),
        Some("not_member")
    );
}
