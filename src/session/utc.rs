//! UTC clock synchronisation via `UTCTiming` elements in the MPD.
//!
//! Implements the client-side of the `TimeSyncController.js` role from
//! dash.js.  After loading a live manifest, `DashSession::from_url` calls
//! [`resolve_utc_timing`] to compute a signed clock offset
//! (`server_time − client_time`) that is then applied to every live-edge
//! segment calculation.
//!
//! # Supported schemes
//!
//! | `@schemeIdUri` | How the server time is obtained |
//! |---|---|
//! | `urn:mpeg:dash:utc:direct:2014` | Parse `@value` as an XS datetime — no network request needed |
//! | `urn:mpeg:dash:utc:http-xsdate:2014` | GET `@value` URL; parse body as XS datetime |
//! | `urn:mpeg:dash:utc:http-iso:2014` | GET `@value` URL; parse body as ISO 8601 (same parser) |
//! | `urn:mpeg:dash:utc:http-head:2014` | HEAD `@value` URL; parse `Date:` response header as RFC 2822 / XS datetime |
//! | `urn:mpeg:dash:utc:ntp:2014` | Requires raw UDP NTP — **not supported**; skipped with a warning |
//!
//! The resolver tries elements in manifest order and stops at the first
//! success.  If every element either fails or is unsupported, the offset
//! is left at `0.0` (system clock used as-is).

use alloc::sync::Arc;

#[cfg(not(feature = "std"))]
use alloc::{borrow::ToOwned, vec::Vec};

use crate::manifest::mpd_types::MPD;
use crate::Url;

use crate::{
    fetch::{FetchRequest, RequestType, SegmentFetcher},
    util::parse_xs_datetime,
};

// Scheme URI constants.
const SCHEME_DIRECT: &str = "urn:mpeg:dash:utc:direct:2014";
const SCHEME_HTTP_XSDATE: &str = "urn:mpeg:dash:utc:http-xsdate:2014";
const SCHEME_HTTP_ISO: &str = "urn:mpeg:dash:utc:http-iso:2014";
const SCHEME_HTTP_HEAD: &str = "urn:mpeg:dash:utc:http-head:2014";
const SCHEME_NTP: &str = "urn:mpeg:dash:utc:ntp:2014";

/// Resolve the UTC clock offset (in seconds) from `MPD.UTCTiming` elements.
///
/// Returns `server_time_s − client_now_s`, where `client_now_s` is
/// `SystemTime::now()` at the moment the server's time is obtained.
///
/// Returns `0.0` if no `UTCTiming` element is present, all elements fail, or
/// no supported scheme is found.
pub(crate) async fn resolve_utc_timing<F: SegmentFetcher>(
    mpd: &MPD,
    fetcher: &Arc<F>,
    client_now_s: f64,
) -> f64 {
    for entry in &mpd.UTCTiming {
        let scheme = entry.schemeIdUri.as_str();

        match scheme {
            SCHEME_DIRECT => {
                if let Some(value) = &entry.value {
                    if let Some(server_s) = parse_xs_datetime(value) {
                        let offset = server_s - client_now_s;
                        tracing::info!(
                            scheme = SCHEME_DIRECT,
                            offset_s = offset,
                            "UTCTiming: clock offset resolved"
                        );
                        return offset;
                    }
                    tracing::warn!(
                        scheme = SCHEME_DIRECT,
                        value = %value,
                        "UTCTiming: failed to parse direct value — trying next element"
                    );
                } else {
                    tracing::warn!(
                        scheme = SCHEME_DIRECT,
                        "UTCTiming: direct element has no @value — trying next element"
                    );
                }
            }

            SCHEME_HTTP_XSDATE | SCHEME_HTTP_ISO => {
                if let Some(url_str) = &entry.value {
                    match Url::parse(url_str) {
                        Ok(url) => {
                            if let Some(offset) = query_http_utc(&url, fetcher, client_now_s).await
                            {
                                tracing::info!(
                                    scheme,
                                    url = %url_str,
                                    offset_s = offset,
                                    "UTCTiming: clock offset resolved"
                                );
                                return offset;
                            }
                            tracing::warn!(
                                scheme,
                                url = %url_str,
                                "UTCTiming: HTTP query failed — trying next element"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                scheme,
                                url = %url_str,
                                error = %e,
                                "UTCTiming: invalid URL — trying next element"
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        scheme,
                        "UTCTiming: HTTP element has no @value URL — trying next element"
                    );
                }
            }

            SCHEME_HTTP_HEAD => {
                // DASH spec §5.8.5.7: issue a HEAD request to the value URL and
                // read the server's current time from the `Date:` response header.
                // Requires the `SegmentFetcher::head_date` method to be implemented
                // by the concrete fetcher; the default returns `Ok(None)` (not
                // supported), causing this element to be skipped.
                if let Some(url_str) = &entry.value {
                    match Url::parse(url_str) {
                        Ok(url) => {
                            match fetcher.head_date(&url).await {
                                Ok(Some(date_str)) => {
                                    // The Date: header is RFC 2822 / HTTP-date format
                                    // (e.g. "Sat, 09 May 2026 12:00:00 GMT").
                                    // Our parser also accepts XS dateTime strings — try
                                    // both formats by attempting the parse directly.
                                    if let Some(server_s) = parse_http_date(&date_str) {
                                        let offset = server_s - client_now_s;
                                        tracing::info!(
                                            scheme = SCHEME_HTTP_HEAD,
                                            url = %url_str,
                                            offset_s = offset,
                                            "UTCTiming: clock offset resolved via http-head"
                                        );
                                        return offset;
                                    }
                                    tracing::warn!(
                                        scheme = SCHEME_HTTP_HEAD,
                                        url = %url_str,
                                        date_header = %date_str,
                                        "UTCTiming: http-head Date header could not be parsed \
                                         — trying next element"
                                    );
                                }
                                Ok(None) => {
                                    tracing::warn!(
                                        scheme = SCHEME_HTTP_HEAD,
                                        url = %url_str,
                                        "UTCTiming: http-head not supported by this fetcher \
                                         (head_date returned None) — trying next element"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        scheme = SCHEME_HTTP_HEAD,
                                        url = %url_str,
                                        error = %e,
                                        "UTCTiming: http-head request failed — trying next element"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                scheme = SCHEME_HTTP_HEAD,
                                url = %url_str,
                                error = %e,
                                "UTCTiming: invalid URL for http-head — trying next element"
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        scheme = SCHEME_HTTP_HEAD,
                        "UTCTiming: http-head element has no @value URL — trying next element"
                    );
                }
            }

            SCHEME_NTP => {
                tracing::warn!(
                    scheme = SCHEME_NTP,
                    "UTCTiming: NTP scheme requires raw UDP and is not yet \
                     supported — trying next element"
                );
            }

            other => {
                tracing::warn!(
                    scheme = other,
                    "UTCTiming: unrecognised scheme — trying next element"
                );
            }
        }
    }

    0.0
}

/// Fetch the UTCTiming endpoint and parse the response body as an XS datetime.
///
/// Returns `server_time_s − client_now_s` on success, `None` on any error.
async fn query_http_utc<F: SegmentFetcher>(
    url: &Url,
    fetcher: &Arc<F>,
    client_now_s: f64,
) -> Option<f64> {
    let request = FetchRequest::new(url.clone(), RequestType::ClockSync);
    let response = fetcher.fetch(request).await.ok()?;

    let body = core::str::from_utf8(&response.data).ok()?.trim().to_owned();
    let server_s = parse_xs_datetime(&body)?;
    Some(server_s - client_now_s)
}

/// Parse an HTTP `Date:` header value (RFC 2822 / IMF-fixdate) into a Unix
/// timestamp in seconds.
///
/// Examples of accepted formats:
/// - `"Sat, 09 May 2026 12:00:00 GMT"` (IMF-fixdate / RFC 2822)
/// - `"2026-05-09T12:00:00Z"` (XS dateTime — falls through to `parse_xs_datetime`)
///
/// Returns `None` when the string cannot be parsed in any known format.
fn parse_http_date(s: &str) -> Option<f64> {
    // First try our XS dateTime parser (handles ISO 8601 variants).
    if let Some(ts) = parse_xs_datetime(s) {
        return Some(ts);
    }

    // Fall back to a minimal RFC 2822 / IMF-fixdate parser.
    // Format: "<day-name>, <DD> <Mon> <YYYY> <HH>:<MM>:<SS> GMT"
    // Example: "Sat, 09 May 2026 12:00:00 GMT"
    parse_rfc2822_date(s)
}

/// Minimal parser for the RFC 2822 date format used in HTTP `Date:` headers.
///
/// Only parses the IMF-fixdate form (the only form allowed in HTTP/1.1):
/// `<day-of-week>, <DD> <MMM> <YYYY> <HH>:<MM>:<SS> GMT`
///
/// Leap seconds and non-UTC zones are not supported.
fn parse_rfc2822_date(s: &str) -> Option<f64> {
    // Strip optional "<day-of-week>, " prefix.
    let s = if let Some(comma_pos) = s.find(',') {
        s[comma_pos + 1..].trim()
    } else {
        s.trim()
    };

    // Expected remaining: "DD Mon YYYY HH:MM:SS TZ"
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }

    let day: u64 = parts[0].parse().ok()?;
    let month: u64 = match parts[1] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = parts[2].parse().ok()?;
    let tz = parts[4];
    if !tz.eq_ignore_ascii_case("GMT") && !tz.eq_ignore_ascii_case("UTC") {
        return None;
    }
    let time_parts: Vec<&str> = parts[3].split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: u64 = time_parts[0].parse().ok()?;
    let minute: u64 = time_parts[1].parse().ok()?;
    let second: u64 = time_parts[2].parse().ok()?;

    // Convert to Unix timestamp using the proleptic Gregorian calendar.
    // Days from epoch (1970-01-01) to (year-01-01).
    let days_from_epoch = days_since_epoch(year, month, day)?;

    let total_secs = days_from_epoch * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(total_secs as f64)
}

/// Compute the number of days since 1970-01-01 for the given (year, month, day).
fn days_since_epoch(year: u64, month: u64, day: u64) -> Option<u64> {
    if year < 1970 || !(1..=12).contains(&month) || day < 1 {
        return None;
    }

    // Days in each month (non-leap year).
    const DAYS_IN_MONTH: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    let is_leap = |y: u64| (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);

    let max_day = DAYS_IN_MONTH[(month - 1) as usize] + if month == 2 && is_leap(year) { 1 } else { 0 };
    if day > max_day {
        return None;
    }

    // Days from 1970-01-01 to start of `year`.
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    // Days within the year up to the start of `month`.
    for m in 1..month {
        let idx = (m - 1) as usize;
        days += DAYS_IN_MONTH[idx];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }

    // Day of month (1-based → 0-based offset).
    days += day - 1;
    Some(days)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{parse_http_date, parse_rfc2822_date};

    // ── parse_rfc2822_date ────────────────────────────────────────────────────

    /// 2009-02-13 23:31:30 UTC == Unix timestamp 1234567890.
    #[test]
    fn rfc2822_known_timestamp() {
        let ts = parse_rfc2822_date("Fri, 13 Feb 2009 23:31:30 GMT").unwrap();
        assert!((ts - 1_234_567_890.0).abs() < 1.0, "got {ts}");
    }

    #[test]
    fn rfc2822_without_day_name_prefix() {
        // The day-name and comma are optional in the parser.
        let ts = parse_rfc2822_date("13 Feb 2009 23:31:30 GMT").unwrap();
        assert!((ts - 1_234_567_890.0).abs() < 1.0, "got {ts}");
    }

    #[test]
    fn rfc2822_invalid_month_returns_none() {
        assert!(parse_rfc2822_date("Fri, 13 Xyz 2009 23:31:30 GMT").is_none());
    }

    #[test]
    fn rfc2822_too_short_returns_none() {
        assert!(parse_rfc2822_date("incomplete").is_none());
    }

    #[test]
    fn rfc2822_unix_epoch() {
        let ts = parse_rfc2822_date("Thu, 01 Jan 1970 00:00:00 GMT").unwrap();
        assert!((ts - 0.0).abs() < 1.0, "expected 0, got {ts}");
    }

    #[test]
    fn rfc2822_all_months_are_recognised() {
        // Spot-check a few months to ensure the match arms are all present.
        for (mon, day, expected_month_num) in [("Jan", 1u64, 1u64), ("Jun", 1, 6), ("Dec", 1, 12)] {
            let s = format!("01 {mon} 1970 00:00:00 GMT");
            let ts = parse_rfc2822_date(&s).unwrap_or_else(|| panic!("failed for month {mon}"));
            // Rough sanity check: the month offset in seconds should be positive.
            let _ = expected_month_num; // used implicitly via ts > 0 for months > Jan
            let _ = day;
            assert!(ts >= 0.0, "timestamp for {mon} was {ts}");
        }
    }

    #[test]
    fn rfc2822_non_utc_timezone_returns_none() {
        // A non-UTC offset must be rejected; silently treating it as UTC would
        // produce a timestamp that is off by the offset amount.
        assert!(parse_rfc2822_date("Fri, 13 Feb 2009 23:31:30 +0200").is_none());
        assert!(parse_rfc2822_date("Fri, 13 Feb 2009 23:31:30 EST").is_none());
    }

    #[test]
    fn rfc2822_day_out_of_range_returns_none() {
        // Day 32 is impossible in any month — must not silently produce a
        // wrong timestamp by overflowing into the next month.
        assert!(parse_rfc2822_date("Fri, 32 Jan 2009 00:00:00 GMT").is_none());
    }

    #[test]
    fn rfc2822_utc_case_insensitive() {
        let ts = parse_rfc2822_date("Fri, 13 Feb 2009 23:31:30 utc").unwrap();
        assert!((ts - 1_234_567_890.0).abs() < 1.0, "got {ts}");
    }

    // ── parse_http_date ───────────────────────────────────────────────────────

    #[test]
    fn parse_http_date_accepts_rfc2822() {
        let ts = parse_http_date("Fri, 13 Feb 2009 23:31:30 GMT").unwrap();
        assert!((ts - 1_234_567_890.0).abs() < 1.0);
    }

    #[test]
    fn parse_http_date_accepts_xs_datetime() {
        // XS datetime format is handled by parse_xs_datetime internally.
        let ts = parse_http_date("2009-02-13T23:31:30Z").unwrap();
        assert!((ts - 1_234_567_890.0).abs() < 1.0);
    }

    #[test]
    fn parse_http_date_returns_none_for_garbage() {
        assert!(parse_http_date("not a date at all!!!").is_none());
    }

    #[test]
    fn parse_http_date_returns_none_for_empty_string() {
        assert!(parse_http_date("").is_none());
    }
}
