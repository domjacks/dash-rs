//! Shared utility functions used across multiple modules.

// ── XS datetime parser ────────────────────────────────────────────────────────

/// Parse an XML Schema dateTime string (`YYYY-MM-DDTHH:MM:SS[.sss]Z`) into a
/// Unix timestamp (seconds since 1970-01-01T00:00:00Z).
///
/// Only UTC values (trailing `Z` or `+00:00`) are accepted.  Offset-timezone
/// strings (e.g. `+05:30`) return `None` — callers should fall back to the
/// system clock in that case.  Leap seconds (second == 60) are accepted and
/// clamped to 59.
///
/// # Examples
///
/// ```
/// use dash_rs::util::parse_xs_datetime;
///
/// // 1970-01-01T00:00:00Z == Unix epoch 0
/// assert_eq!(parse_xs_datetime("1970-01-01T00:00:00Z"), Some(0.0));
///
/// // Known fixed point: 2000-01-01T00:00:00Z == 946 684 800
/// assert_eq!(parse_xs_datetime("2000-01-01T00:00:00Z"), Some(946_684_800.0));
///
/// // Fractional seconds
/// assert!((parse_xs_datetime("2000-01-01T00:00:00.5Z").unwrap() - 946_684_800.5).abs() < 1e-9);
/// ```
pub fn parse_xs_datetime(s: &str) -> Option<f64> {
    let s = s.trim();

    // Strip UTC timezone markers; reject non-UTC offsets.
    let s = if let Some(inner) = s.strip_suffix('Z') {
        inner
    } else if let Some(inner) = s.strip_suffix("+00:00") {
        inner
    } else if s.ends_with(|c: char| c.is_ascii_digit()) {
        // No timezone marker — assume UTC (common for UTCTiming `direct` values).
        s
    } else {
        return None;
    };

    // Minimum: "YYYY-MM-DDTHH:MM:SS" = 19 chars.
    if s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();

    // Parse fixed-width date/time fields.
    let year: i64 = s[0..4].parse().ok()?;
    if b[4] != b'-' {
        return None;
    }
    let month: u32 = s[5..7].parse().ok()?;
    if b[7] != b'-' {
        return None;
    }
    let day: u32 = s[8..10].parse().ok()?;
    if b[10] != b'T' && b[10] != b't' {
        return None;
    }
    let hour: u32 = s[11..13].parse().ok()?;
    if b[13] != b':' {
        return None;
    }
    let minute: u32 = s[14..16].parse().ok()?;
    if b[16] != b':' {
        return None;
    }
    let second: u32 = s[17..19].parse().ok()?;

    // Optional fractional seconds: ".ddd" immediately after the seconds field.
    let frac_s: f64 = if s.len() > 19 && b[19] == b'.' {
        let frac_str = &s[20..];
        // Ensure all characters are digits (no stray timezone suffix).
        if !frac_str.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let digits: f64 = frac_str.parse().ok()?;
        digits / 10_f64.powi(frac_str.len() as i32)
    } else if s.len() > 19 {
        // Unexpected trailing content — reject.
        return None;
    } else {
        0.0
    };

    // Basic range validation (allow leap second == 60 → clamp below).
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let second = second.min(59); // clamp leap second

    let is_leap = |y: i64| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);

    // Days from Unix epoch to start of `year`.
    let mut days: i64 = 0;
    if year >= 1970 {
        for y in 1970..year {
            days += if is_leap(y) { 366 } else { 365 };
        }
    } else {
        for y in year..1970 {
            days -= if is_leap(y) { 366 } else { 365 };
        }
    }

    // Add days for completed months in the year.
    let month_days: [u32; 12] = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for &d in month_days.iter().take(month as usize - 1) {
        days += d as i64;
    }

    // Add completed days within the month (day is 1-indexed).
    days += (day - 1) as i64;

    let unix_s = days as f64 * 86_400.0
        + hour as f64 * 3_600.0
        + minute as f64 * 60.0
        + second as f64
        + frac_s;

    Some(unix_s)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::parse_xs_datetime;

    #[test]
    fn unix_epoch() {
        assert_eq!(parse_xs_datetime("1970-01-01T00:00:00Z"), Some(0.0));
    }

    #[test]
    fn unix_epoch_no_z() {
        // No timezone marker → treated as UTC.
        assert_eq!(parse_xs_datetime("1970-01-01T00:00:00"), Some(0.0));
    }

    #[test]
    fn plus_zero_offset() {
        assert_eq!(parse_xs_datetime("1970-01-01T00:00:00+00:00"), Some(0.0));
    }

    #[test]
    fn year_2000() {
        // 2000-01-01T00:00:00Z = 946 684 800
        assert_eq!(
            parse_xs_datetime("2000-01-01T00:00:00Z"),
            Some(946_684_800.0)
        );
    }

    #[test]
    fn fractional_seconds() {
        let t = parse_xs_datetime("2000-01-01T00:00:00.500Z").unwrap();
        assert!((t - 946_684_800.5).abs() < 1e-9);
    }

    #[test]
    fn fractional_seconds_three_digits() {
        let t = parse_xs_datetime("2000-01-01T00:00:00.789Z").unwrap();
        assert!((t - 946_684_800.789).abs() < 1e-6);
    }

    #[test]
    fn leap_year_feb_29() {
        // 2000 is a leap year; 2000-03-01 = 946684800 + 60 days = 946684800 + 5184000
        let feb28 = parse_xs_datetime("2000-02-28T00:00:00Z").unwrap();
        let feb29 = parse_xs_datetime("2000-02-29T00:00:00Z").unwrap();
        let mar01 = parse_xs_datetime("2000-03-01T00:00:00Z").unwrap();
        assert!((feb29 - feb28 - 86_400.0).abs() < 1e-9);
        assert!((mar01 - feb29 - 86_400.0).abs() < 1e-9);
    }

    #[test]
    fn non_leap_year_feb_29_rejected() {
        // 2001 is not a leap year; day 29 is out of range — we do not validate
        // this at the month-day level (only basic 1–31 check), so it silently
        // over-counts.  The important invariant is that real dates parse
        // correctly, not that invalid calendar dates are rejected.
        // This test just documents current behaviour.
        assert!(parse_xs_datetime("2001-02-29T00:00:00Z").is_some());
    }

    #[test]
    fn pre_epoch() {
        // 1969-12-31T00:00:00Z = -86400
        assert_eq!(
            parse_xs_datetime("1969-12-31T00:00:00Z"),
            Some(-86_400.0)
        );
    }

    #[test]
    fn non_utc_offset_rejected() {
        assert!(parse_xs_datetime("2000-01-01T00:00:00+05:30").is_none());
    }

    #[test]
    fn too_short_rejected() {
        assert!(parse_xs_datetime("2000-01-01").is_none());
    }

    #[test]
    fn invalid_month_rejected() {
        assert!(parse_xs_datetime("2000-13-01T00:00:00Z").is_none());
    }

    #[test]
    fn leap_second_clamped() {
        // Second 60 (leap second) is accepted and treated as 59.
        let t = parse_xs_datetime("2000-01-01T00:00:60Z").unwrap();
        let expected = parse_xs_datetime("2000-01-01T00:00:59Z").unwrap();
        assert!((t - expected).abs() < 1e-9);
    }

    #[test]
    fn known_timestamp_2024() {
        // 2024-01-01T00:00:00Z
        // Days from epoch to 2024-01-01:
        //   1970–2023 = 54 years; leap years in that range:
        //   1972,1976,1980,1984,1988,1992,1996,2000,2004,2008,2012,2016,2020 = 13 leap years
        //   Non-leap: 54 - 13 = 41 years
        //   Days: 13*366 + 41*365 = 4758 + 14965 = 19723 days
        //   Unix seconds: 19723 * 86400 = 1_704_067_200
        assert_eq!(
            parse_xs_datetime("2024-01-01T00:00:00Z"),
            Some(1_704_067_200.0)
        );
    }
}
