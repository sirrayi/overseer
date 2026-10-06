//! Minimal RFC 3339 → unix-ms conversion (days-from-civil), enough for the
//! `resets_at`/`timestamp` fields provider APIs return. Rejects malformed
//! input instead of panicking.

/// Days since the unix epoch for a proleptic-Gregorian y/m/d
/// (Howard Hinnant's days_from_civil).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn is_leap_second_ok(sec: u32) -> bool {
    // RFC 3339 permits 60 at leap-second boundaries; we clamp rather than
    // fail since providers occasionally emit it.
    sec <= 60
}

fn take_digits(s: &[u8], n: usize) -> Option<(u32, &[u8])> {
    if s.len() < n || !s[..n].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut v = 0u32;
    for b in &s[..n] {
        v = v * 10 + u32::from(b - b'0');
    }
    Some((v, &s[n..]))
}

/// Parse an RFC 3339 timestamp (`2026-10-05T12:34:56.789Z`,
/// `+02:30`/`Z` offsets required) into unix epoch milliseconds.
/// Returns None for anything malformed or out of range.
pub fn parse_rfc3339_ms(input: &str) -> Option<i64> {
    let s = input.trim().as_bytes();
    let (year, rest) = take_digits(s, 4)?;
    if rest.first() != Some(&b'-') {
        return None;
    }
    let (month, rest) = take_digits(&rest[1..], 2)?;
    if rest.first() != Some(&b'-') {
        return None;
    }
    let (day, rest) = take_digits(&rest[1..], 2)?;
    if !(rest.first() == Some(&b'T') || rest.first() == Some(&b't')) {
        return None;
    }
    let (hour, rest) = take_digits(&rest[1..], 2)?;
    if rest.first() != Some(&b':') {
        return None;
    }
    let (minute, rest) = take_digits(&rest[1..], 2)?;
    if rest.first() != Some(&b':') {
        return None;
    }
    let (second, mut rest) = take_digits(&rest[1..], 2)?;

    let mut frac_ms: i64 = 0;
    if rest.first() == Some(&b'.') {
        rest = &rest[1..];
        let mut digits = 0usize;
        let mut frac = 0i64;
        while digits < rest.len() && rest[digits].is_ascii_digit() {
            if digits < 3 {
                frac = frac * 10 + i64::from(rest[digits] - b'0');
            }
            digits += 1;
        }
        if digits == 0 {
            return None;
        }
        frac_ms = match digits {
            1 => frac * 100,
            2 => frac * 10,
            _ => frac, // 3+ digits: keep millisecond precision
        };
        rest = &rest[digits..];
    }

    let offset_min: i64 = match rest.first() {
        Some(&b'Z') | Some(&b'z') => {
            if rest.len() != 1 {
                return None;
            }
            0
        }
        Some(&b'+') | Some(&b'-') => {
            let sign = if rest[0] == b'-' { -1i64 } else { 1i64 };
            let (oh, r) = take_digits(&rest[1..], 2)?;
            if r.first() != Some(&b':') {
                return None;
            }
            let (om, r) = take_digits(&r[1..], 2)?;
            if !r.is_empty() || oh > 23 || om > 59 {
                return None;
            }
            sign * (oh as i64 * 60 + om as i64)
        }
        _ => return None,
    };

    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || !is_leap_second_ok(second) {
        return None;
    }
    if day == 0 || day > days_in_month(year as i64, month) {
        return None;
    }
    let second = second.min(59); // leap second → :59

    let days = days_from_civil(year as i64, month, day);
    let secs =
        days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64 - offset_min * 60;
    Some(secs * 1000 + frac_ms)
}

/// Unix seconds field → ms (None for non-positive/non-finite).
pub fn unix_seconds_to_ms(value: Option<f64>) -> Option<i64> {
    value
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| (v * 1000.0) as i64)
}

/// Unix milliseconds field → ms.
pub fn unix_millis_to_ms(value: Option<f64>) -> Option<i64> {
    value
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v as i64)
}

/// ISO-8601-ish string field → ms via the RFC 3339 parser.
pub fn iso_to_ms(value: Option<&serde_json::Value>) -> Option<i64> {
    value
        .and_then(crate::snapshot::as_str)
        .and_then(parse_rfc3339_ms)
}

/// UTC day number (days since epoch) — the cost ledger's rollover key.
pub fn utc_day(now_ms: i64) -> i64 {
    now_ms.div_euclid(86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2000-02-29T12:00:00Z"),
            Some(951_825_600_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-10-05T00:00:00Z"),
            Some(1_791_158_400_000)
        );
    }

    #[test]
    fn fractional_seconds() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.05Z"), Some(50));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.005Z"), Some(5));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.123456Z"), Some(123));
    }

    #[test]
    fn offsets() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_rfc3339_ms("1969-12-31T19:00:00-05:00"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2026-10-05T02:30:00+02:30"),
            Some(1_791_158_400_000)
        );
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "not a date",
            "2026-13-01T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-10-05 12:00:00",
            "2026-10-05T25:00:00Z",
            "2026-10-05T12:00:00",
            "2026-10-05T12:00:00+25:00",
            "2026-10-05T12:00:00.abcZ",
            "2026-10-05T12:00:00ZZ",
        ] {
            assert_eq!(parse_rfc3339_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn leap_second_clamps() {
        assert_eq!(
            parse_rfc3339_ms("2016-12-31T23:59:60Z"),
            Some(1_483_228_799_000)
        );
    }

    #[test]
    fn utc_day_rollovers() {
        assert_eq!(utc_day(0), 0);
        assert_eq!(utc_day(86_399_999), 0);
        assert_eq!(utc_day(86_400_000), 1);
        assert_eq!(utc_day(-1), -1);
    }
}
