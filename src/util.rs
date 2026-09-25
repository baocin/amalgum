//! Small shared helpers: wall-clock time, compact relative times, stable hashing, dates.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Compact relative time as shown in rows ("now", "45s", "4m", "2h", "3d", "5w", "2y").
pub fn relative_time(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..=4 => "now".into(),
        5..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m", s / 60),
        3_600..=86_399 => format!("{}h", s / 3_600),
        86_400..=604_799 => format!("{}d", s / 86_400),
        604_800..=31_535_999 => format!("{}w", s / 604_800),
        _ => format!("{}y", s / 31_536_000),
    }
}

/// FNV-1a 64-bit. Stable across runs, platforms, and releases, so it may key on-disk
/// state (repo ids) and user-visible choices (lane colors). Never change it.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

/// Parse `YYYY-MM-DD` (UTC midnight) into Unix seconds.
pub fn parse_date(s: &str) -> Option<u64> {
    let mut it = s.splitn(3, '-');
    let (y, m, d): (i64, u32, u32) =
        (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    u64::try_from(days_from_civil(y, m, d) * 86_400).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_buckets() {
        assert_eq!(relative_time(100, 100), "now");
        assert_eq!(relative_time(100, 50), "50s");
        assert_eq!(relative_time(4 * 60, 0), "4m");
        assert_eq!(relative_time(2 * 3_600 + 5, 0), "2h");
        assert_eq!(relative_time(3 * 86_400, 0), "3d");
        assert_eq!(relative_time(0, 100), "now", "future timestamps clamp to now");
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2000-03-01"), Some(951_868_800));
        assert_eq!(parse_date("2024-13-01"), None);
        assert_eq!(parse_date("yesterday"), None);
    }
}
