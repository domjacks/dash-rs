//! MPEG-DASH MPD type definitions.
//!
//! Vendored equivalents of the `dash-mpd` crate's public types — only the
//! fields actually accessed by the rest of the library are declared.  Unknown
//! XML elements and attributes are silently ignored during deserialization.
//!
//! All field names match their `dash-mpd` equivalents verbatim so that
//! call-site code required no renaming.  The `#[allow(non_snake_case)]`
//! annotation suppresses Rust naming-convention warnings for the deliberately
//! PascalCase field names (e.g. `SegmentTemplate`, `UTCTiming`).

#![allow(non_snake_case)]

use std::time::Duration;

use serde::Deserialize;

// ── MPD root ──────────────────────────────────────────────────────────────────

/// The root `<MPD>` element.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct MPD {
    /// `@type`: `"static"` (VOD) or `"dynamic"` (live).  Absent implies `"static"`.
    #[serde(rename = "@type")]
    pub mpdtype: Option<String>,

    /// `@mediaPresentationDuration` — total VOD duration (ISO 8601 duration).
    #[serde(rename = "@mediaPresentationDuration", deserialize_with = "de::opt_duration")]
    pub mediaPresentationDuration: Option<Duration>,

    /// `@minimumUpdatePeriod` — how often the live manifest should be refreshed.
    #[serde(rename = "@minimumUpdatePeriod", deserialize_with = "de::opt_duration")]
    pub minimumUpdatePeriod: Option<Duration>,

    /// `@availabilityStartTime` — Unix epoch seconds when content became available.
    ///
    /// Stored as `f64` seconds (instead of `DateTime<Utc>`) to avoid a `chrono`
    /// dependency and to remain `wasm32`-friendly.
    #[serde(rename = "@availabilityStartTime", deserialize_with = "de::opt_xs_datetime")]
    pub availabilityStartTime: Option<f64>,

    /// `@suggestedPresentationDelay` — recommended presentation delay for live streams.
    #[serde(rename = "@suggestedPresentationDelay", deserialize_with = "de::opt_duration")]
    pub suggestedPresentationDelay: Option<Duration>,

    /// `<Period>` children.
    #[serde(rename = "Period")]
    pub periods: Vec<Period>,

    /// `<BaseURL>` children.
    #[serde(rename = "BaseURL")]
    pub base_url: Vec<BaseURL>,

    /// `<Location>` children — alternative manifest URLs used on refresh.
    #[serde(rename = "Location")]
    pub locations: Vec<Location>,

    /// `<UTCTiming>` children — server clock synchronisation.
    #[serde(rename = "UTCTiming")]
    pub UTCTiming: Vec<UTCTiming>,

    /// `<ServiceDescription>` children — low-latency live stream parameters.
    #[serde(rename = "ServiceDescription")]
    pub ServiceDescription: Vec<ServiceDescription>,
}

// ── Period ────────────────────────────────────────────────────────────────────

/// A `<Period>` element.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Period {
    #[serde(rename = "@id")]
    pub id: Option<String>,

    #[serde(rename = "@start", deserialize_with = "de::opt_duration")]
    pub start: Option<Duration>,

    #[serde(rename = "@duration", deserialize_with = "de::opt_duration")]
    pub duration: Option<Duration>,

    /// `<AdaptationSet>` children.
    #[serde(rename = "AdaptationSet")]
    pub adaptations: Vec<AdaptationSet>,

    /// `<BaseURL>` children.
    #[serde(rename = "BaseURL")]
    pub BaseURL: Vec<BaseURL>,

    #[serde(rename = "SegmentTemplate")]
    pub SegmentTemplate: Option<SegmentTemplate>,

    #[serde(rename = "SegmentList")]
    pub SegmentList: Option<SegmentList>,

    #[serde(rename = "SegmentBase")]
    pub SegmentBase: Option<SegmentBase>,

    /// `xlink:href` — URL of a remote XLink resource (resolved when `actuate="onLoad"`).
    #[serde(rename = "@href")]
    pub href: Option<String>,

    /// `xlink:actuate` — `"onLoad"` or `"onRequest"`.
    #[serde(rename = "@actuate")]
    pub actuate: Option<String>,
}

// ── AdaptationSet ─────────────────────────────────────────────────────────────

/// An `<AdaptationSet>` element.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct AdaptationSet {
    #[serde(rename = "@id")]
    pub id: Option<String>,

    /// `@contentType` — `"video"`, `"audio"`, `"text"`, etc.
    #[serde(rename = "@contentType")]
    pub contentType: Option<String>,

    #[serde(rename = "@mimeType")]
    pub mimeType: Option<String>,

    #[serde(rename = "@lang")]
    pub lang: Option<String>,

    /// `<Representation>` children.
    #[serde(rename = "Representation")]
    pub representations: Vec<Representation>,

    /// `<BaseURL>` children.
    #[serde(rename = "BaseURL")]
    pub BaseURL: Vec<BaseURL>,

    #[serde(rename = "SegmentTemplate")]
    pub SegmentTemplate: Option<SegmentTemplate>,

    #[serde(rename = "SegmentList")]
    pub SegmentList: Option<SegmentList>,

    #[serde(rename = "SegmentBase")]
    pub SegmentBase: Option<SegmentBase>,

    #[serde(rename = "@href")]
    pub href: Option<String>,

    #[serde(rename = "@actuate")]
    pub actuate: Option<String>,
}

// ── Representation ────────────────────────────────────────────────────────────

/// A `<Representation>` element.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Representation {
    #[serde(rename = "@id")]
    pub id: Option<String>,

    #[serde(rename = "@bandwidth")]
    pub bandwidth: Option<u64>,

    #[serde(rename = "@mimeType")]
    pub mimeType: Option<String>,

    #[serde(rename = "@codecs")]
    pub codecs: Option<String>,

    #[serde(rename = "@width")]
    pub width: Option<u64>,

    #[serde(rename = "@height")]
    pub height: Option<u64>,

    /// `<BaseURL>` children.
    #[serde(rename = "BaseURL")]
    pub BaseURL: Vec<BaseURL>,

    #[serde(rename = "SegmentTemplate")]
    pub SegmentTemplate: Option<SegmentTemplate>,

    #[serde(rename = "SegmentList")]
    pub SegmentList: Option<SegmentList>,

    #[serde(rename = "SegmentBase")]
    pub SegmentBase: Option<SegmentBase>,
}

// ── SegmentTemplate ───────────────────────────────────────────────────────────

/// A `<SegmentTemplate>` element.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SegmentTemplate {
    /// `@media` — URL template for media segments (contains `$Number$` etc.).
    #[serde(rename = "@media")]
    pub media: Option<String>,

    /// `@initialization` attribute — URL template for the init segment.
    #[serde(rename = "@initialization")]
    pub initialization: Option<String>,

    #[serde(rename = "@startNumber")]
    pub startNumber: Option<u64>,

    /// `@duration` — segment duration in timescale ticks (stored as `f64` for
    /// convenience in division expressions).
    #[serde(rename = "@duration")]
    pub duration: Option<f64>,

    #[serde(rename = "@timescale")]
    pub timescale: Option<u64>,

    #[serde(rename = "@presentationTimeOffset")]
    pub presentationTimeOffset: Option<u64>,

    /// `<Initialization>` child element — alternative to the `@initialization` attribute.
    #[serde(rename = "Initialization")]
    pub Initialization: Option<Initialization>,

    #[serde(rename = "SegmentTimeline")]
    pub SegmentTimeline: Option<SegmentTimeline>,
}

// ── SegmentTimeline / S ───────────────────────────────────────────────────────

/// A `<SegmentTimeline>` element — container for `<S>` entries.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SegmentTimeline {
    #[serde(rename = "S")]
    pub segments: Vec<S>,
}

/// An `<S>` entry within a `<SegmentTimeline>`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct S {
    /// `@t` — segment start time in timescale ticks (inferred from predecessor when absent).
    #[serde(rename = "@t")]
    pub t: Option<u64>,

    /// `@d` — segment duration in timescale ticks.
    #[serde(rename = "@d")]
    pub d: u64,

    /// `@r` — repeat count (`-1` = infinite sliding window for live streams).
    #[serde(rename = "@r")]
    pub r: Option<i64>,
}

// ── SegmentList ───────────────────────────────────────────────────────────────

/// A `<SegmentList>` element.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SegmentList {
    /// `@duration` — segment duration in timescale ticks.
    #[serde(rename = "@duration")]
    pub duration: Option<u64>,

    #[serde(rename = "@timescale")]
    pub timescale: Option<u64>,

    #[serde(rename = "@startNumber")]
    pub startNumber: Option<u64>,

    #[serde(rename = "@presentationTimeOffset")]
    pub presentationTimeOffset: Option<u64>,

    /// `<SegmentURL>` children.
    #[serde(rename = "SegmentURL")]
    pub segment_urls: Vec<SegmentURL>,

    #[serde(rename = "Initialization")]
    pub Initialization: Option<Initialization>,

    #[serde(rename = "SegmentTimeline")]
    pub SegmentTimeline: Option<SegmentTimeline>,
}

// ── SegmentURL ────────────────────────────────────────────────────────────────

/// A `<SegmentURL>` entry within a `<SegmentList>`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SegmentURL {
    #[serde(rename = "@media")]
    pub media: Option<String>,

    #[serde(rename = "@mediaRange")]
    pub mediaRange: Option<String>,
}

// ── SegmentBase ───────────────────────────────────────────────────────────────

/// A `<SegmentBase>` element — byte-range segment addressing.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SegmentBase {
    #[serde(rename = "@timescale")]
    pub timescale: Option<u64>,

    #[serde(rename = "@presentationTimeOffset")]
    pub presentationTimeOffset: Option<u64>,

    /// `@indexRange` — byte range for the `sidx` box (e.g. `"200-456"`).
    #[serde(rename = "@indexRange")]
    pub indexRange: Option<String>,

    #[serde(rename = "Initialization")]
    pub Initialization: Option<Initialization>,
}

// ── Initialization ────────────────────────────────────────────────────────────

/// An `<Initialization>` element.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Initialization {
    /// `@sourceURL` — URL of a separate init-segment file.
    #[serde(rename = "@sourceURL")]
    pub sourceURL: Option<String>,

    /// `@range` — byte range within the media file for the init data.
    #[serde(rename = "@range")]
    pub range: Option<String>,
}

// ── BaseURL ───────────────────────────────────────────────────────────────────

/// A `<BaseURL>` element.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct BaseURL {
    /// Text content — the URL string (may be absolute or relative).
    #[serde(rename = "$text")]
    pub base: String,

    /// `@serviceLocation` — CDN identity key used for blacklisting.
    #[serde(rename = "@serviceLocation")]
    pub serviceLocation: Option<String>,

    /// `@priority` — lower value = higher priority (DASH §5.10.1.3).
    #[serde(rename = "@priority")]
    pub priority: Option<u64>,

    /// `@weight` — relative weight for load balancing within the same priority tier.
    #[serde(rename = "@weight")]
    pub weight: Option<u64>,
}

// ── Location ──────────────────────────────────────────────────────────────────

/// A `<Location>` element — alternative manifest URL for refresh fallback.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Location {
    /// Text content — the URL string.
    #[serde(rename = "$text")]
    pub url: String,
}

// ── UTCTiming ─────────────────────────────────────────────────────────────────

/// A `<UTCTiming>` element — server clock synchronisation descriptor.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct UTCTiming {
    /// `@schemeIdUri` — identifies the clock scheme (e.g. `urn:mpeg:dash:utc:direct:2014`).
    #[serde(rename = "@schemeIdUri")]
    pub schemeIdUri: String,

    /// `@value` — scheme-specific value (URL or inline XS dateTime).
    #[serde(rename = "@value")]
    pub value: Option<String>,
}

// ── ServiceDescription ────────────────────────────────────────────────────────

/// A `<ServiceDescription>` element (low-latency live stream parameters).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ServiceDescription {
    #[serde(rename = "Latency")]
    pub Latency: Vec<Latency>,

    #[serde(rename = "PlaybackRate")]
    pub PlaybackRate: Vec<PlaybackRate>,
}

/// `<Latency>` element within `<ServiceDescription>`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Latency {
    /// `@target` — target presentation latency in seconds.
    #[serde(rename = "@target")]
    pub target: Option<f64>,

    #[serde(rename = "@max")]
    pub max: Option<f64>,

    #[serde(rename = "@min")]
    pub min: Option<f64>,
}

/// `<PlaybackRate>` element within `<ServiceDescription>`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct PlaybackRate {
    #[serde(rename = "@max")]
    pub max: Option<f64>,

    #[serde(rename = "@min")]
    pub min: Option<f64>,
}

// ── Private serde helpers ─────────────────────────────────────────────────────

pub(super) mod de {
    use std::time::Duration;

    /// Deserialise an optional ISO 8601 duration attribute.
    ///
    /// Called by `#[serde(deserialize_with = "de::opt_duration")]`.  When the
    /// attribute is absent `serde`'s `default` machinery returns `None`
    /// directly — this function is only invoked when the attribute is present.
    pub fn opt_duration<'de, D>(de: D) -> Result<Option<Duration>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s: String = serde::Deserialize::deserialize(de)?;
        if s.is_empty() {
            return Ok(None);
        }
        parse_iso_duration(&s)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid ISO 8601 duration: {s}")))
    }

    /// Deserialise an optional XS dateTime attribute to a Unix timestamp (seconds).
    ///
    /// Called by `#[serde(deserialize_with = "de::opt_xs_datetime")]`.
    pub fn opt_xs_datetime<'de, D>(de: D) -> Result<Option<f64>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s: String = serde::Deserialize::deserialize(de)?;
        if s.is_empty() {
            return Ok(None);
        }
        crate::util::parse_xs_datetime(&s)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid xs:dateTime: {s}")))
    }

    /// Parse an ISO 8601 duration string into a `std::time::Duration`.
    ///
    /// Supported format: `P[nY][nM][nW][nD][T[nH][nM][n[.f]S]]`
    ///
    /// Years and months are approximated (365.25 days and 30.4375 days
    /// respectively) — acceptable for DASH presentation-duration purposes.
    pub fn parse_iso_duration(s: &str) -> Option<Duration> {
        let s = s.trim();
        if !s.starts_with('P') {
            return None;
        }

        let mut total_secs: f64 = 0.0;
        let mut chars = s[1..].chars().peekable(); // skip leading 'P'
        let mut in_time_part = false;

        while chars.peek().is_some() {
            // 'T' switches from date designators to time designators.
            if chars.peek() == Some(&'T') {
                in_time_part = true;
                chars.next();
                continue;
            }

            // Collect digits and optional decimal point.
            let mut num_str = String::new();
            while matches!(chars.peek(), Some('0'..='9') | Some('.')) {
                num_str.push(chars.next().unwrap());
            }
            if num_str.is_empty() {
                break; // malformed — stop gracefully
            }

            let num: f64 = num_str.parse().ok()?;
            let designator = chars.next()?;

            match (in_time_part, designator) {
                (false, 'Y') => total_secs += num * 365.25 * 86_400.0,
                (false, 'M') => total_secs += num * 30.4375 * 86_400.0,
                (false, 'W') => total_secs += num * 7.0 * 86_400.0,
                (false, 'D') => total_secs += num * 86_400.0,
                (true, 'H') => total_secs += num * 3_600.0,
                (true, 'M') => total_secs += num * 60.0,
                (true, 'S') => total_secs += num,
                _ => {} // ignore unknown designators
            }
        }

        Some(Duration::from_secs_f64(total_secs))
    }

    // ── Unit tests ────────────────────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::parse_iso_duration;
        use std::time::Duration;

        #[test]
        fn pt0s() {
            assert_eq!(parse_iso_duration("PT0S"), Some(Duration::ZERO));
        }

        #[test]
        fn pt30s() {
            assert_eq!(parse_iso_duration("PT30S"), Some(Duration::from_secs(30)));
        }

        #[test]
        fn pt5m30s() {
            assert_eq!(parse_iso_duration("PT5M30S"), Some(Duration::from_secs(330)));
        }

        #[test]
        fn pt1h() {
            assert_eq!(parse_iso_duration("PT1H"), Some(Duration::from_secs(3_600)));
        }

        #[test]
        fn p1d() {
            assert_eq!(parse_iso_duration("P1D"), Some(Duration::from_secs(86_400)));
        }

        #[test]
        fn p1dt2h30m() {
            assert_eq!(
                parse_iso_duration("P1DT2H30M"),
                Some(Duration::from_secs(86_400 + 7_200 + 1_800))
            );
        }

        #[test]
        fn fractional_seconds() {
            assert_eq!(
                parse_iso_duration("PT1.5S"),
                Some(Duration::from_millis(1_500))
            );
        }

        #[test]
        fn empty_returns_none() {
            assert_eq!(parse_iso_duration(""), None);
        }

        #[test]
        fn no_p_prefix_returns_none() {
            assert_eq!(parse_iso_duration("30S"), None);
        }
    }
}
