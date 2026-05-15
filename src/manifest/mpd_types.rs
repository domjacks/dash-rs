//! MPEG-DASH MPD type definitions.
//!
//! Vendored equivalents of the `dash-mpd` crate's public types — only the
//! fields actually accessed by the rest of the library are declared.  Unknown
//! XML elements and attributes are silently ignored during parsing.
//!
//! All field names match their `dash-mpd` equivalents verbatim so that
//! call-site code required no renaming.  The `#[allow(non_snake_case)]`
//! annotation suppresses Rust naming-convention warnings for the deliberately
//! PascalCase field names (e.g. `SegmentTemplate`, `UTCTiming`).

#![allow(non_snake_case)]

use core::time::Duration;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

// ── MPD root ──────────────────────────────────────────────────────────────────

/// The root `<MPD>` element.
#[derive(Debug, Default, Clone)]
pub struct MPD {
    /// `@type`: `"static"` (VOD) or `"dynamic"` (live).  Absent implies `"static"`.
    pub mpdtype: Option<String>,

    /// `@profiles` — comma-separated list of DASH profiles (informational).
    pub profiles: Option<String>,

    /// `@mediaPresentationDuration` — total VOD duration (ISO 8601 duration).
    pub mediaPresentationDuration: Option<Duration>,

    /// `@minimumUpdatePeriod` — how often the live manifest should be refreshed.
    pub minimumUpdatePeriod: Option<Duration>,

    /// `@minBufferTime` — minimum time of data a client should have buffered
    /// before starting playback (ISO 8601 duration, DASH spec §5.3.1.2).
    pub minBufferTime: Option<Duration>,

    /// `@availabilityStartTime` — Unix epoch seconds when content became available.
    ///
    /// Stored as `f64` seconds (instead of `DateTime<Utc>`) to avoid a `chrono`
    /// dependency and to remain `wasm32`-friendly.
    pub availabilityStartTime: Option<f64>,

    /// `@availabilityEndTime` — Unix epoch seconds when the live stream ends.
    ///
    /// When present, `DashSession::fetch_next_segment` returns
    /// [`DashError::EndOfStream`] as soon as the adjusted wall clock exceeds
    /// this value, allowing cleanly scheduled broadcasts (sport events, etc.)
    /// to terminate without spurious 404 errors (DASH spec §5.3.1.2).
    pub availabilityEndTime: Option<f64>,

    /// `@suggestedPresentationDelay` — recommended presentation delay for live streams.
    pub suggestedPresentationDelay: Option<Duration>,

    /// `@timeShiftBufferDepth` — maximum DVR window depth for live streams
    /// (ISO 8601 duration, DASH spec §5.3.1.2).
    ///
    /// The presentation time of segments older than
    /// `availabilityStartTime + (now - timeShiftBufferDepth)` is outside the
    /// DVR window and those segments may no longer be available on the CDN.
    /// `DashSession::fetch_next_segment` advances `segment_number` past the
    /// DVR floor automatically and warns when content has expired.
    pub timeShiftBufferDepth: Option<Duration>,

    /// `<Period>` children.
    pub periods: Vec<Period>,

    /// `<BaseURL>` children.
    pub base_url: Vec<BaseURL>,

    /// `<Location>` children — alternative manifest URLs used on refresh.
    pub locations: Vec<Location>,

    /// `<UTCTiming>` children — server clock synchronisation.
    pub UTCTiming: Vec<UTCTiming>,

    /// `<ServiceDescription>` children — low-latency live stream parameters.
    pub ServiceDescription: Vec<ServiceDescription>,
}

// ── Period ────────────────────────────────────────────────────────────────────

/// A `<Period>` element.
#[derive(Debug, Default, Clone)]
pub struct Period {
    pub id: Option<String>,
    pub start: Option<Duration>,
    pub duration: Option<Duration>,

    /// `<AdaptationSet>` children.
    pub adaptations: Vec<AdaptationSet>,

    /// `<BaseURL>` children.
    pub BaseURL: Vec<BaseURL>,

    pub SegmentTemplate: Option<SegmentTemplate>,
    pub SegmentList: Option<SegmentList>,
    pub SegmentBase: Option<SegmentBase>,

    /// `xlink:href` — URL of a remote XLink resource (resolved when `actuate="onLoad"`).
    pub href: Option<String>,

    /// `xlink:actuate` — `"onLoad"` or `"onRequest"`.
    pub actuate: Option<String>,
}

// ── AdaptationSet ─────────────────────────────────────────────────────────────

/// An `<AdaptationSet>` element.
#[derive(Debug, Default, Clone)]
pub struct AdaptationSet {
    pub id: Option<String>,

    /// `@contentType` — `"video"`, `"audio"`, `"text"`, etc.
    pub contentType: Option<String>,

    pub mimeType: Option<String>,
    pub lang: Option<String>,

    /// `@codecs` — codec string shared by all representations in the set.
    ///
    /// When present at the `AdaptationSet` level all representations use the
    /// same codec; individual `Representation@codecs` attributes override this
    /// per-representation (DASH spec §5.3.7.2).
    pub codecs: Option<String>,

    /// `<Representation>` children.
    pub representations: Vec<Representation>,

    /// `<BaseURL>` children.
    pub BaseURL: Vec<BaseURL>,

    pub SegmentTemplate: Option<SegmentTemplate>,
    pub SegmentList: Option<SegmentList>,
    pub SegmentBase: Option<SegmentBase>,

    pub href: Option<String>,
    pub actuate: Option<String>,
}

// ── Representation ────────────────────────────────────────────────────────────

/// A `<Representation>` element.
#[derive(Debug, Default, Clone)]
pub struct Representation {
    pub id: Option<String>,
    pub bandwidth: Option<u64>,
    pub mimeType: Option<String>,
    pub codecs: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,

    /// `<BaseURL>` children.
    pub BaseURL: Vec<BaseURL>,

    pub SegmentTemplate: Option<SegmentTemplate>,
    pub SegmentList: Option<SegmentList>,
    pub SegmentBase: Option<SegmentBase>,
}

// ── SegmentTemplate ───────────────────────────────────────────────────────────

/// A `<SegmentTemplate>` element.
#[derive(Debug, Default, Clone)]
pub struct SegmentTemplate {
    /// `@media` — URL template for media segments (contains `$Number$` etc.).
    pub media: Option<String>,

    /// `@initialization` attribute — URL template for the init segment.
    pub initialization: Option<String>,

    pub startNumber: Option<u64>,

    /// `@duration` — segment duration in timescale ticks (stored as `f64` for
    /// convenience in division expressions).
    pub duration: Option<f64>,

    pub timescale: Option<u64>,
    pub presentationTimeOffset: Option<u64>,

    /// `@availabilityTimeOffset` — seconds a segment becomes available *before* its
    /// nominal availability time, enabling low-latency CMAF chunk delivery
    /// (DASH spec §5.3.9.4, DASH-LL profile).
    ///
    /// A non-zero value allows the client to start fetching a segment before it
    /// is fully produced.  `compute_live_edge_segment` adds this offset to
    /// `elapsed` so that partially-produced segments at the live edge are
    /// included in the fetch window.
    pub availabilityTimeOffset: Option<f64>,

    /// `@availabilityTimeComplete` — when `false`, the segment is available for
    /// download before it is fully written (chunked transfer / CMAF chunk delivery).
    ///
    /// Parsed and stored for future use; full chunked-segment support
    /// (streaming body) is deferred to P1-4.
    pub availabilityTimeComplete: Option<bool>,

    /// `<Initialization>` child element — alternative to the `@initialization` attribute.
    pub Initialization: Option<Initialization>,

    pub SegmentTimeline: Option<SegmentTimeline>,
}

// ── SegmentTimeline / S ───────────────────────────────────────────────────────

/// A `<SegmentTimeline>` element — container for `<S>` entries.
#[derive(Debug, Default, Clone)]
pub struct SegmentTimeline {
    pub segments: Vec<S>,
}

/// An `<S>` entry within a `<SegmentTimeline>`.
#[derive(Debug, Default, Clone)]
pub struct S {
    /// `@t` — segment start time in timescale ticks (inferred from predecessor when absent).
    pub t: Option<u64>,

    /// `@d` — segment duration in timescale ticks.
    pub d: u64,

    /// `@r` — repeat count (`-1` = infinite sliding window for live streams).
    pub r: Option<i64>,
}

// ── SegmentList ───────────────────────────────────────────────────────────────

/// A `<SegmentList>` element.
#[derive(Debug, Default, Clone)]
pub struct SegmentList {
    /// `@duration` — segment duration in timescale ticks.
    pub duration: Option<u64>,

    pub timescale: Option<u64>,
    pub startNumber: Option<u64>,
    pub presentationTimeOffset: Option<u64>,

    /// `<SegmentURL>` children.
    pub segment_urls: Vec<SegmentURL>,

    pub Initialization: Option<Initialization>,
    pub SegmentTimeline: Option<SegmentTimeline>,
}

// ── SegmentURL ────────────────────────────────────────────────────────────────

/// A `<SegmentURL>` entry within a `<SegmentList>`.
#[derive(Debug, Default, Clone)]
pub struct SegmentURL {
    pub media: Option<String>,
    pub mediaRange: Option<String>,
}

// ── SegmentBase ───────────────────────────────────────────────────────────────

/// A `<SegmentBase>` element — byte-range segment addressing.
#[derive(Debug, Default, Clone)]
pub struct SegmentBase {
    pub timescale: Option<u64>,
    pub presentationTimeOffset: Option<u64>,

    /// `@indexRange` — byte range for the `sidx` box (e.g. `"200-456"`).
    pub indexRange: Option<String>,

    pub Initialization: Option<Initialization>,
}

// ── Initialization ────────────────────────────────────────────────────────────

/// An `<Initialization>` element.
#[derive(Debug, Default, Clone)]
pub struct Initialization {
    /// `@sourceURL` — URL of a separate init-segment file.
    pub sourceURL: Option<String>,

    /// `@range` — byte range within the media file for the init data.
    pub range: Option<String>,
}

// ── BaseURL ───────────────────────────────────────────────────────────────────

/// A `<BaseURL>` element.
#[derive(Debug, Default, Clone)]
pub struct BaseURL {
    /// Text content — the URL string (may be absolute or relative).
    pub base: String,

    /// `@serviceLocation` — CDN identity key used for blacklisting.
    pub serviceLocation: Option<String>,

    /// `@priority` — lower value = higher priority (DASH §5.10.1.3).
    pub priority: Option<u64>,

    /// `@weight` — relative weight for load balancing within the same priority tier.
    pub weight: Option<u64>,
}

// ── Location ──────────────────────────────────────────────────────────────────

/// A `<Location>` element — alternative manifest URL for refresh fallback.
#[derive(Debug, Default, Clone)]
pub struct Location {
    /// Text content — the URL string.
    pub url: String,
}

// ── UTCTiming ─────────────────────────────────────────────────────────────────

/// A `<UTCTiming>` element — server clock synchronisation descriptor.
#[derive(Debug, Default, Clone)]
pub struct UTCTiming {
    /// `@schemeIdUri` — identifies the clock scheme (e.g. `urn:mpeg:dash:utc:direct:2014`).
    pub schemeIdUri: String,

    /// `@value` — scheme-specific value (URL or inline XS dateTime).
    pub value: Option<String>,
}

// ── ServiceDescription ────────────────────────────────────────────────────────

/// A `<ServiceDescription>` element (low-latency live stream parameters).
#[derive(Debug, Default, Clone)]
pub struct ServiceDescription {
    pub Latency: Vec<Latency>,
    pub PlaybackRate: Vec<PlaybackRate>,
}

/// `<Latency>` element within `<ServiceDescription>`.
#[derive(Debug, Default, Clone)]
pub struct Latency {
    /// `@target` — target presentation latency in seconds.
    pub target: Option<f64>,
    pub max: Option<f64>,
    pub min: Option<f64>,
}

/// `<PlaybackRate>` element within `<ServiceDescription>`.
#[derive(Debug, Default, Clone)]
pub struct PlaybackRate {
    pub max: Option<f64>,
    pub min: Option<f64>,
}
