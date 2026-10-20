//! Calendar arithmetic on UTC dates, enough to say "expires in 176 days"
//! and "last seen 2026-09-07" without a date-time crate.

use std::time::{SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`]: `(year, month, day)`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DD` for a Unix time.
pub fn ymd(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DDTHH:MM:SSZ` (UTC) for a Unix time.
pub fn utc_timestamp(unix: u64) -> String {
    let secs = unix % 86_400;
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        ymd(unix),
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// The date part of an RFC 3339 string such as `2027-03-02T23:03:00Z`.
pub fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let s = s.get(..10)?;
    let mut it = s.split('-');
    let y = it.next()?.parse().ok()?;
    let m = it.next()?.parse().ok()?;
    let d = it.next()?.parse().ok()?;
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then_some((y, m, d))
}

/// Whole days from today until the RFC 3339 instant `s`; negative once it
/// has passed. `None` when it cannot be read or is a zero time, which is how
/// Tailscale writes "never".
pub fn days_until(s: &str) -> Option<i64> {
    let (y, m, d) = parse_ymd(s)?;
    if y < 2000 {
        return None;
    }
    let today = (now_unix() / 86_400) as i64;
    Some(days_from_civil(y, m, d) - today)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trips_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_999), (2024, 10, 3));
        for days in [-1, 1, 59, 60, 365, 20_000, 30_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(ymd(1_788_739_200), "2026-09-07");
    }

    #[test]
    fn rfc3339_dates_are_read_and_zero_time_means_never() {
        assert_eq!(parse_ymd("2027-03-02T23:03:00Z"), Some((2027, 3, 2)));
        assert_eq!(parse_ymd("2027-13-02T00:00:00Z"), None);
        assert_eq!(parse_ymd("soon"), None);
        assert_eq!(days_until("0001-01-01T00:00:00Z"), None);
        assert!(days_until("2999-01-01T00:00:00Z").unwrap() > 300_000);
        assert!(days_until("2001-01-01T00:00:00Z").unwrap() < 0);
    }

    #[test]
    fn a_unix_time_reads_as_utc() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(1_792_258_000), "2026-10-17T17:26:40Z");
    }
}
