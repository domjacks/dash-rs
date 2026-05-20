//! Platform-agnostic media buffer abstraction.
//!
//! [`MediaBuffer`] is the single integration point through which
//! [`crate::session::DashSession`] delivers media bytes to a platform sink —
//! a browser MSE [`SourceBuffer`][mse], a GStreamer `appsrc`, an in-memory
//! stub, or anything else.
//!
//! All methods have default (no-op / empty) implementations so you only need
//! to override what your platform actually requires.
//!
//! # Provided implementations
//!
//! | Type | Use for |
//! |---|---|
//! | [`NullBuffer`] | Headless tests, benchmarks — silently drops all data |
//! | [`StubBuffer`] | Tests that inspect appended data or timing state |
//!
//! # Required annotation: `#[async_trait(?Send)]`
//!
//! This trait uses `async-trait` and **must** be implemented with `?Send`:
//!
//! ```rust,ignore
//! use async_trait::async_trait;
//! use dash_rs::{MediaBuffer, MediaType};
//!
//! struct MyBuffer;
//!
//! #[async_trait(?Send)]
//! impl MediaBuffer for MyBuffer {
//!     async fn append(&mut self, _mt: MediaType, data: bytes::Bytes, _is_init: bool)
//!         -> Result<(), dash_rs::DashError>
//!     {
//!         // forward data to your platform decoder
//!         Ok(())
//!     }
//! }
//! ```
//!
//! [mse]: https://www.w3.org/TR/media-source/

extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{error::DashError, session::MediaType};

// ── TimeRange ────────────────────────────────────────────────────────────────

/// A half-open interval `[start, end)` of buffered media time in seconds.
///
/// Returned by [`MediaBuffer::buffered`] and consumed by the ABR controller
/// and gap detector.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeRange {
    /// Start of the buffered range, in seconds.
    pub start: f64,
    /// End of the buffered range, in seconds.
    pub end: f64,
}

impl TimeRange {
    /// Construct a new range.
    pub fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }

    /// Duration of the range in seconds.
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }
}

// ── MediaBuffer trait ────────────────────────────────────────────────────────

/// Platform-agnostic interface for receiving and tracking media data.
///
/// See the [module-level docs][self] for usage and the required
/// `#[async_trait(?Send)]` annotation.
#[async_trait(?Send)]
pub trait MediaBuffer {
    // ── Lifecycle ────────────────────────────────────────────────────────

    /// Prepare the sink to receive media data.
    ///
    /// For browser MSE implementations this should call
    /// `URL.createObjectURL(new MediaSource())`, attach it to the
    /// `<video>` element, and await the `sourceopen` event.
    /// For most native sinks this can be a no-op.
    async fn open(&mut self) -> Result<(), DashError> {
        Ok(())
    }

    /// Notify the sink of the total presentation duration in seconds.
    ///
    /// Called for VOD streams with the `MPD@mediaPresentationDuration` value.
    /// Live streams pass `f64::INFINITY`.
    fn set_duration(&mut self, _secs: f64) {}

    /// Notify the sink that the stream is live.
    ///
    /// GStreamer implementations use this to install a PTS-subtraction pad
    /// probe.  Most other sinks can ignore it.
    fn set_live_mode(&mut self, _live: bool) {}

    /// Signal that no more media data will be appended.
    async fn end_of_stream(&mut self) -> Result<(), DashError> {
        Ok(())
    }

    // ── Track setup ──────────────────────────────────────────────────────

    /// Request that the sink allocate a decoder / pipeline track for `mt`.
    ///
    /// `mime_type` is the MIME string computed from the MPD
    /// (e.g. `"video/mp4; codecs=\"avc1.64001f\""` or `"audio/mp4; codecs=\"mp4a.40.2\""`).
    ///
    /// Return `false` to reject the track — the session will skip it entirely.
    /// Browser implementations should delegate to
    /// `MediaSource.isTypeSupported(mime_type)` before calling
    /// `addSourceBuffer`.
    fn add_track(&mut self, _mt: MediaType, _mime_type: &str) -> bool {
        true
    }

    /// Change the codec of an existing track at a period boundary.
    ///
    /// Corresponds to `SourceBuffer.changeType()` in the MSE spec.
    async fn change_type(&mut self, _mt: MediaType, _mime_type: &str) -> Result<(), DashError> {
        Ok(())
    }

    // ── Timing ───────────────────────────────────────────────────────────

    /// Set the decode-timestamp offset for `mt`.
    ///
    /// The library calls this once at session startup and again at period
    /// boundaries when the offset changes.  Corresponds to
    /// `SourceBuffer.timestampOffset` in the MSE spec.
    fn set_timestamp_offset(&mut self, _mt: MediaType, _offset_s: f64) {}

    /// Set the append window `[start, end)` for `mt`.
    ///
    /// The library calls this once at session startup and again at period
    /// boundaries.  Corresponds to `SourceBuffer.appendWindowStart/End` in
    /// the MSE spec.  Non-MSE sinks can ignore it.
    fn set_append_window(&mut self, _mt: MediaType, _start: f64, _end: f64) {}

    // ── Data delivery ────────────────────────────────────────────────────

    /// Append a media chunk to the buffer for track `mt`.
    ///
    /// `is_init` is `true` for initialisation segments (fMP4 `moov` boxes)
    /// and `false` for media segments.  Init segments arrive first and may
    /// be re-sent after a codec change.
    async fn append(
        &mut self,
        _mt: MediaType,
        _data: Bytes,
        _is_init: bool,
    ) -> Result<(), DashError> {
        Ok(())
    }

    /// Remove the buffered range `[start, end)` for track `mt`.
    ///
    /// Called by the buffer-pruning logic when `PlaybackDriver::enable_pruning`
    /// returns `true`.
    async fn remove_range(
        &mut self,
        _mt: MediaType,
        _start: f64,
        _end: f64,
    ) -> Result<(), DashError> {
        Ok(())
    }

    // ── Buffer state ─────────────────────────────────────────────────────

    /// Return the currently buffered time ranges for track `mt`.
    ///
    /// Used by the ABR controller (to compute buffer level) and the gap
    /// detector.  If this returns an empty `Vec` the library treats the buffer
    /// level as zero — ABR will behave conservatively.
    ///
    /// Browser implementations should read `SourceBuffer.buffered`.
    /// GStreamer implementations accumulate ranges locally after each append.
    /// [`NullBuffer`] and headless sinks that do not need ABR accuracy can
    /// return `vec![]`.
    fn buffered(&self, _mt: MediaType) -> Vec<TimeRange> {
        alloc::vec![]
    }

    /// Returns `true` when [`buffered`][Self::buffered] provides meaningful
    /// time-range data.
    ///
    /// When this returns `false`, [`DashSession::current_metrics`] substitutes
    /// the session's own [`estimated_buffer_level_ms`] in place of the value
    /// derived from `buffered()`, so that buffer-occupancy ABR rules
    /// ([`InsufficientBufferRule`], [`BufferOccupancyRule`]) receive a
    /// realistic level rather than a permanent zero.
    ///
    /// **Override to `false` for sinks that cannot report buffered ranges**,
    /// e.g. a GStreamer `appsrc` pipeline where data is consumed opaquely by
    /// the renderer with no MSE-equivalent `SourceBuffer.buffered` API.
    ///
    /// [`estimated_buffer_level_ms`]: crate::session::DashSession::estimated_buffer_level_ms
    /// [`InsufficientBufferRule`]: crate::abr::insufficient_buffer::InsufficientBufferRule
    /// [`BufferOccupancyRule`]: crate::abr::buffer::BufferOccupancyRule
    fn has_buffer_tracking(&self) -> bool {
        true
    }
}

// ── NullBuffer ───────────────────────────────────────────────────────────────

/// A [`MediaBuffer`] that silently discards all data.
///
/// All methods use the default no-op implementations.  `buffered()` always
/// returns an empty list, so the ABR controller will always see zero buffer
/// level (meaning it will tend toward conservative quality selection).
///
/// Use `NullBuffer` for:
/// - Unit and integration tests that only care about segment-fetch counts,
///   quality decisions, or end-of-stream behaviour.
/// - Headless benchmarks.
/// - Offline manifest-analysis tools.
#[derive(Debug, Default, Clone)]
pub struct NullBuffer;

#[async_trait(?Send)]
impl MediaBuffer for NullBuffer {}

// ── StubBuffer ───────────────────────────────────────────────────────────────

/// An in-memory [`MediaBuffer`] that records everything the library sends.
///
/// Useful for tests that need to assert on:
/// - Which segments were appended (`video_chunks` / `audio_chunks`)
/// - The current timestamp offset or append window
/// - Whether `end_of_stream` was called
/// - The current MIME type after a `change_type` call
#[derive(Debug, Default)]
pub struct StubBuffer {
    /// Chunks appended for the video track (init + media, in arrival order).
    pub video_chunks: alloc::vec::Vec<StubChunk>,
    /// Chunks appended for the audio track.
    pub audio_chunks: alloc::vec::Vec<StubChunk>,
    /// Current `timestampOffset` for the video track.
    pub video_timestamp_offset: f64,
    /// Current `timestampOffset` for the audio track.
    pub audio_timestamp_offset: f64,
    /// Current append window `(start, end)` for the video track.
    pub video_append_window: (f64, f64),
    /// Current append window `(start, end)` for the audio track.
    pub audio_append_window: (f64, f64),
    /// Current MIME type for the video track (set by `add_track` /
    /// `change_type`).
    pub video_mime: alloc::string::String,
    /// Current MIME type for the audio track.
    pub audio_mime: alloc::string::String,
    /// Value last passed to `set_duration`.
    pub duration: Option<f64>,
    /// `true` after `end_of_stream` is called.
    pub ended: bool,
    /// `true` after `set_live_mode(true)` is called.
    pub is_live: bool,
}

/// A single appended chunk recorded by [`StubBuffer`].
#[derive(Debug, Clone)]
pub struct StubChunk {
    /// Raw bytes of the segment.
    pub data: Bytes,
    /// `true` for init segments, `false` for media segments.
    pub is_init: bool,
}

impl StubBuffer {
    /// Create a new, empty stub buffer.
    pub fn new() -> Self {
        Self {
            video_append_window: (0.0, f64::INFINITY),
            audio_append_window: (0.0, f64::INFINITY),
            ..Default::default()
        }
    }

    /// Total bytes appended to the video track.
    pub fn video_bytes_appended(&self) -> usize {
        self.video_chunks.iter().map(|c| c.data.len()).sum()
    }

    /// Total bytes appended to the audio track.
    pub fn audio_bytes_appended(&self) -> usize {
        self.audio_chunks.iter().map(|c| c.data.len()).sum()
    }
}

#[async_trait(?Send)]
impl MediaBuffer for StubBuffer {
    fn set_duration(&mut self, secs: f64) {
        self.duration = Some(secs);
    }

    fn set_live_mode(&mut self, live: bool) {
        self.is_live = live;
    }

    async fn end_of_stream(&mut self) -> Result<(), DashError> {
        self.ended = true;
        Ok(())
    }

    fn add_track(&mut self, mt: MediaType, mime_type: &str) -> bool {
        match mt {
            MediaType::Video => self.video_mime = mime_type.into(),
            MediaType::Audio => self.audio_mime = mime_type.into(),
        }
        true
    }

    async fn change_type(&mut self, mt: MediaType, mime_type: &str) -> Result<(), DashError> {
        match mt {
            MediaType::Video => self.video_mime = mime_type.into(),
            MediaType::Audio => self.audio_mime = mime_type.into(),
        }
        Ok(())
    }

    fn set_timestamp_offset(&mut self, mt: MediaType, offset_s: f64) {
        match mt {
            MediaType::Video => self.video_timestamp_offset = offset_s,
            MediaType::Audio => self.audio_timestamp_offset = offset_s,
        }
    }

    fn set_append_window(&mut self, mt: MediaType, start: f64, end: f64) {
        match mt {
            MediaType::Video => self.video_append_window = (start, end),
            MediaType::Audio => self.audio_append_window = (start, end),
        }
    }

    async fn append(&mut self, mt: MediaType, data: Bytes, is_init: bool) -> Result<(), DashError> {
        let chunk = StubChunk { data, is_init };
        match mt {
            MediaType::Video => self.video_chunks.push(chunk),
            MediaType::Audio => self.audio_chunks.push(chunk),
        }
        Ok(())
    }

    async fn remove_range(
        &mut self,
        mt: MediaType,
        _start: f64,
        _end: f64,
    ) -> Result<(), DashError> {
        // For the stub we don't track timing, so removal is a no-op.
        let _ = mt;
        Ok(())
    }

    /// Always returns `vec![]` — `StubBuffer` does not parse fMP4 timing.
    fn buffered(&self, _mt: MediaType) -> Vec<TimeRange> {
        alloc::vec![]
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::TimeRange;

    #[test]
    fn time_range_duration_normal() {
        let r = TimeRange::new(1.0, 5.0);
        assert!((r.duration() - 4.0).abs() < 1e-9);
    }

    #[test]
    fn time_range_duration_zero_length() {
        let r = TimeRange::new(3.0, 3.0);
        assert!((r.duration() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn time_range_duration_inverted_is_negative() {
        // Callers must not construct inverted ranges, but the struct itself
        // does not panic — it returns a negative duration so the ABR controller
        // and gap detector can detect the anomaly.
        let r = TimeRange::new(5.0, 1.0);
        assert!(r.duration() < 0.0, "inverted range should yield negative duration");
    }

    #[test]
    fn time_range_duration_nan_propagates() {
        let r = TimeRange::new(f64::NAN, 5.0);
        assert!(r.duration().is_nan(), "NaN start should propagate through duration()");
    }
}
