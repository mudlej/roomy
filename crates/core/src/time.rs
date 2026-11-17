//! Times written for people: in reports and log lines.

use std::time::{SystemTime, UNIX_EPOCH};

/// Formats a time as `YYYY-MM-DD HH:MM` in UTC.
pub(crate) fn utc_timestamp(time: SystemTime) -> String {
    let secs = seconds_since_epoch(time) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3_600, rem % 3_600 / 60)
}

/// UTC date and time to the second, for log lines.
pub(crate) fn utc_timestamp_secs(time: SystemTime) -> String {
    format!("{}:{:02}", utc_timestamp(time), seconds_since_epoch(time) % 60)
}

fn seconds_since_epoch(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps_are_utc_calendar_dates() {
        let at = |secs| utc_timestamp(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), "1970-01-01 00:00");
        assert_eq!(at(951_782_400), "2000-02-29 00:00");
        assert_eq!(at(1_794_866_290), "2026-11-16 21:58");
        assert_eq!(at(1_798_761_599), "2026-12-31 23:59");
        assert_eq!(utc_timestamp_secs(UNIX_EPOCH + Duration::from_secs(1_794_866_290)), "2026-11-16 21:58:10");
    }
}
