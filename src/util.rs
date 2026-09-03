//! Small clock/date helpers.

use chrono::{Local, TimeZone, Utc};

pub fn unix_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// Unix ms of local midnight today — the boundary for `!today`.
/// This machine's current UTC offset in minutes. The database groups days in
/// local time (the streamer's), so anything that re-derives a day elsewhere —
/// the site, in a reader's browser — needs the same offset to agree.
pub fn local_utc_offset_minutes() -> i64 {
    Local::now().offset().local_minus_utc() as i64 / 60
}

pub fn local_day_start_ms() -> i64 {
    let now = Local::now();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| Local.from_local_datetime(&dt).single())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|| now.timestamp_millis() - 86_400_000)
}

pub fn date_of_ms(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => "?".into(),
    }
}

pub fn datetime_of_ms(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => "?".into(),
    }
}
