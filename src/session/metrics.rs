//! Formal DASH playback metrics — P2-9.
//!
//! [`DashMetrics`] accumulates three kinds of records during a session:
//!
//! | Type | Description |
//! |---|---|
//! | [`HttpRequestRecord`] | One entry per completed `fetch_next_segment` call |
//! | [`RepresentationSwitchRecord`] | One entry per quality switch |
//! | [`BufferLevelSample`] | One buffer-level snapshot per fetch (at decision time) |
//!
//! Each list is a bounded ring buffer capped at [`METRICS_CAPACITY`] entries
//! (matching dash.js `MAX_SAVED_METRICS_REQUESTS = 100`).  When the buffer is
//! full the oldest entry is evicted before inserting a new one.
//!
//! Timestamps (`wall_ms`) are absolute Unix milliseconds
//! (`clock.unix_s() * 1000 + utc_offset_s * 1000`) so they can be correlated
//! with server-side access logs.
//!
//! # Relationship to `AbrMetrics`
//!
//! [`crate::abr::AbrMetrics`] is a *caller-supplied* snapshot used as ABR
//! input; it is not a history.  `DashMetrics` is the accumulated history that
//! the session populates automatically.

use alloc::collections::VecDeque;

#[cfg(not(feature = "std"))]
use alloc::string::String;

use crate::{abr::AbrDecisionReason, session::MediaType};

/// Maximum number of entries kept per metric list.
///
/// Matches `dash.js MAX_SAVED_METRICS_REQUESTS`.
pub const METRICS_CAPACITY: usize = 100;

// ── Record types ──────────────────────────────────────────────────────────────

/// A completed HTTP segment request.
///
/// Populated after every successful [`DashSession::fetch_next_segment`] call.
/// Mirrors the `HttpList` entry in dash.js `DashMetrics.js`.
#[derive(Debug, Clone)]
pub struct HttpRequestRecord {
    /// Absolute wall-clock time in Unix milliseconds when the fetch *started*.
    pub wall_ms: f64,
    /// Which media track was fetched.
    pub media_type: MediaType,
    /// The segment number that was fetched.
    pub segment_number: u64,
    /// The resolved URL that was requested (final URL after redirects, if any).
    pub url: String,
    /// Number of bytes received in the response body.
    pub bytes_received: u64,
    /// Total transfer time, in milliseconds (time-to-last-byte minus
    /// time-to-first-byte is not distinguished here; this is wall time).
    pub transfer_duration_ms: f64,
    /// Computed download throughput, in bits per second.
    /// `None` when `transfer_duration_ms == 0`.
    pub throughput_bps: Option<f64>,
    /// Index into the representation list (sorted ascending by bitrate) that
    /// was active when this segment was fetched.
    pub quality_index: usize,
}

/// A representation (quality) switch event.
///
/// Populated whenever the ABR decision changes the active quality level.
/// Mirrors the `RepSwitchList` entry in dash.js `DashMetrics.js`.
#[derive(Debug, Clone)]
pub struct RepresentationSwitchRecord {
    /// Absolute wall-clock time in Unix milliseconds when the switch occurred.
    pub wall_ms: f64,
    /// Which media track switched quality.
    pub media_type: MediaType,
    /// The representation index that was active *before* the switch.
    pub from_index: usize,
    /// The representation index that became active *after* the switch.
    pub to_index: usize,
    /// The ABR rule that triggered the switch.
    pub reason: AbrDecisionReason,
}

/// A point-in-time buffer level observation.
///
/// Recorded once per `fetch_next_segment` call, at ABR decision time (before
/// the fetch) so that the buffer level reflects the state that drove the
/// quality decision.  Mirrors the `BufferLevel` entry in dash.js
/// `DashMetrics.js`.
#[derive(Debug, Clone)]
pub struct BufferLevelSample {
    /// Absolute wall-clock time in Unix milliseconds.
    pub wall_ms: f64,
    /// Which media track was measured.
    pub media_type: MediaType,
    /// Playback buffer depth in milliseconds at this point in time.
    pub level_ms: f64,
}

// ── DashMetrics container ─────────────────────────────────────────────────────

/// Accumulated playback quality metrics for the current session.
///
/// Returned by [`DashSession::metrics()`].  Each list is a bounded ring buffer
/// of up to [`METRICS_CAPACITY`] entries; older entries are evicted
/// automatically when the cap is reached.
#[derive(Debug, Default)]
pub struct DashMetrics {
    /// Completed HTTP segment requests, oldest first.
    pub http_requests: VecDeque<HttpRequestRecord>,
    /// Representation (quality) switch events, oldest first.
    pub representation_switches: VecDeque<RepresentationSwitchRecord>,
    /// Buffer level snapshots taken at ABR decision time, oldest first.
    pub buffer_level_samples: VecDeque<BufferLevelSample>,
}

impl DashMetrics {
    /// Creates a new, empty `DashMetrics`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an HTTP request record, evicting the oldest if at capacity.
    pub(crate) fn push_http(&mut self, record: HttpRequestRecord) {
        if self.http_requests.len() == METRICS_CAPACITY {
            self.http_requests.pop_front();
        }
        self.http_requests.push_back(record);
    }

    /// Appends a representation-switch record, evicting the oldest if at capacity.
    pub(crate) fn push_switch(&mut self, record: RepresentationSwitchRecord) {
        if self.representation_switches.len() == METRICS_CAPACITY {
            self.representation_switches.pop_front();
        }
        self.representation_switches.push_back(record);
    }

    /// Appends a buffer-level sample, evicting the oldest if at capacity.
    pub(crate) fn push_buffer_level(&mut self, sample: BufferLevelSample) {
        if self.buffer_level_samples.len() == METRICS_CAPACITY {
            self.buffer_level_samples.pop_front();
        }
        self.buffer_level_samples.push_back(sample);
    }

    /// Returns the most recent HTTP request record, or `None` if no fetches
    /// have been recorded yet.
    pub fn last_http_request(&self) -> Option<&HttpRequestRecord> {
        self.http_requests.back()
    }

    /// Returns the most recent representation-switch record, or `None` if no
    /// quality switch has occurred in this session.
    pub fn last_switch(&self) -> Option<&RepresentationSwitchRecord> {
        self.representation_switches.back()
    }

    /// Returns the most recent buffer-level sample, or `None` if no fetch has
    /// been recorded yet.
    pub fn last_buffer_level(&self) -> Option<&BufferLevelSample> {
        self.buffer_level_samples.back()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn http_rec(seg: u64) -> HttpRequestRecord {
        HttpRequestRecord {
            wall_ms: seg as f64 * 1000.0,
            media_type: MediaType::Video,
            segment_number: seg,
            url: format!("https://example.com/seg{seg}.m4s"),
            bytes_received: 4096,
            transfer_duration_ms: 50.0,
            throughput_bps: Some(4096_f64 * 8.0 * 1000.0 / 50.0), // 655360 bps
            quality_index: 0,
        }
    }

    fn switch_rec(from: usize, to: usize) -> RepresentationSwitchRecord {
        RepresentationSwitchRecord {
            wall_ms: 1_000.0,
            media_type: MediaType::Video,
            from_index: from,
            to_index: to,
            reason: AbrDecisionReason::ThroughputBased,
        }
    }

    fn buf_sample(level: f64) -> BufferLevelSample {
        BufferLevelSample {
            wall_ms: 1_000.0,
            media_type: MediaType::Video,
            level_ms: level,
        }
    }

    #[test]
    fn new_metrics_are_empty() {
        let m = DashMetrics::new();
        assert!(m.http_requests.is_empty());
        assert!(m.representation_switches.is_empty());
        assert!(m.buffer_level_samples.is_empty());
        assert!(m.last_http_request().is_none());
        assert!(m.last_switch().is_none());
        assert!(m.last_buffer_level().is_none());
    }

    #[test]
    fn push_http_stores_and_retrieves_record() {
        let mut m = DashMetrics::new();
        m.push_http(http_rec(1));
        assert_eq!(m.http_requests.len(), 1);
        assert_eq!(m.last_http_request().unwrap().segment_number, 1);
    }

    #[test]
    fn push_http_ring_buffer_caps_at_capacity() {
        let mut m = DashMetrics::new();
        for i in 0..=METRICS_CAPACITY as u64 {
            m.push_http(http_rec(i));
        }
        // Must stay at capacity.
        assert_eq!(m.http_requests.len(), METRICS_CAPACITY);
        // Oldest (seg 0) must have been evicted; newest is at the back.
        assert_eq!(
            m.last_http_request().unwrap().segment_number,
            METRICS_CAPACITY as u64
        );
        // Front is seg 1 (seg 0 was evicted).
        assert_eq!(m.http_requests.front().unwrap().segment_number, 1);
    }

    #[test]
    fn push_switch_stores_record() {
        let mut m = DashMetrics::new();
        m.push_switch(switch_rec(0, 2));
        assert_eq!(m.representation_switches.len(), 1);
        let s = m.last_switch().unwrap();
        assert_eq!(s.from_index, 0);
        assert_eq!(s.to_index, 2);
    }

    #[test]
    fn push_buffer_level_stores_sample() {
        let mut m = DashMetrics::new();
        m.push_buffer_level(buf_sample(8_000.0));
        assert_eq!(m.buffer_level_samples.len(), 1);
        assert!((m.last_buffer_level().unwrap().level_ms - 8_000.0).abs() < 1e-9);
    }

    #[test]
    fn ring_buffer_eviction_order_is_oldest_first() {
        let mut m = DashMetrics::new();
        // Push exactly capacity+1 switches.
        for i in 0..=METRICS_CAPACITY {
            m.push_switch(RepresentationSwitchRecord {
                wall_ms: i as f64,
                media_type: MediaType::Video,
                from_index: i,
                to_index: i + 1,
                reason: AbrDecisionReason::ThroughputBased,
            });
        }
        assert_eq!(m.representation_switches.len(), METRICS_CAPACITY);
        // Entry 0 (from_index=0) should have been evicted.
        assert_eq!(m.representation_switches.front().unwrap().from_index, 1);
    }
}
