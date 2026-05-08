pub mod sink;
pub mod stub;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::DashError;

// ── Shared types ─────────────────────────────────────────────────────────────

/// Ready state of the [`MediaSource`], mirroring the `readyState` attribute
/// of the browser's `MediaSource` API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyState {
    /// No source buffers are attached and no session is active.
    Closed,
    /// The source is open and accepting data.
    Open,
    /// [`MediaSource::end_of_stream`] has been called.
    Ended,
}

/// A contiguous range of media time, in seconds.
///
/// Equivalent to one entry returned by the browser's `TimeRanges` object.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeRange {
    pub start: f64,
    pub end: f64,
}

impl TimeRange {
    pub fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }

    /// Duration of this range in seconds.
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }
}

/// A unit of media data together with its presentation time range.
///
/// Passed to [`SourceBuffer::append_buffer`]. `start` and `end` are optional
/// because init segments do not carry a presentation time.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// The raw media bytes (e.g., an MP4 fragment or a WebM cluster).
    pub data: Bytes,
    /// Presentation start time in seconds, if known.
    pub start: Option<f64>,
    /// Presentation end time in seconds, if known.
    pub end: Option<f64>,
    /// Human-readable label for logging (e.g. `"video/init"`, `"audio/seg-3"`).
    pub label: String,
}

impl Chunk {
    pub fn new(
        data: Bytes,
        start: Option<f64>,
        end: Option<f64>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            data,
            start,
            end,
            label: label.into(),
        }
    }
}

// ── SourceBuffer trait ────────────────────────────────────────────────────────

/// Abstraction over the browser MSE `SourceBuffer` object.
///
/// Implementations handle storage and tracking of appended media chunks. The
/// stub implementation ([`stub::StubSourceBuffer`]) stores chunks in memory
/// and immediately resolves every async call without delay, making it suitable
/// for tests and offline tooling.
///
/// A real implementation targeting, for example, GStreamer `appsrc` or a
/// custom demuxer would implement this trait and be injected via
/// [`MediaSource::add_source_buffer`].
#[async_trait(?Send)]
pub trait SourceBuffer {
    /// The MIME type string this buffer was created for (e.g.
    /// `"video/mp4;codecs=\"avc1.42E01E\""`)
    fn mime_type(&self) -> &str;

    /// Whether an `append_buffer` or `remove` operation is currently in
    /// progress. Mirrors `SourceBuffer.updating` from the MSE spec.
    fn is_updating(&self) -> bool;

    /// Returns the contiguous time ranges that have been buffered.
    /// Adjacent or overlapping ranges are merged, matching the MSE
    /// `TimeRanges` semantics from the HTML spec.
    fn buffered(&self) -> Vec<TimeRange>;

    /// Presentation timestamp offset applied to all appended data (seconds).
    /// Mirrors `SourceBuffer.timestampOffset`.
    fn timestamp_offset(&self) -> f64;
    fn set_timestamp_offset(&mut self, offset: f64);

    /// The lower bound of the append window (seconds).
    /// Mirrors `SourceBuffer.appendWindowStart`.
    fn append_window_start(&self) -> f64;
    fn set_append_window_start(&mut self, start: f64);

    /// The upper bound of the append window (seconds).
    /// Mirrors `SourceBuffer.appendWindowEnd`.
    fn append_window_end(&self) -> f64;
    fn set_append_window_end(&mut self, end: f64);

    /// Append a chunk of media data to the buffer.
    ///
    /// Resolves once the append is complete (equivalent to waiting for the
    /// `updateend` event in the browser). Returns an error if the buffer is
    /// currently updating.
    async fn append_buffer(&mut self, chunk: Chunk) -> Result<(), DashError>;

    /// Remove buffered data in the range `[start, end)` (seconds).
    ///
    /// Resolves once the removal is complete.
    async fn remove(&mut self, start: f64, end: f64) -> Result<(), DashError>;

    /// Abort any in-progress append. Resets the parser state.
    /// Mirrors `SourceBuffer.abort()`.
    async fn abort(&mut self) -> Result<(), DashError>;

    /// Switch the MIME type of this source buffer without removing buffered
    /// data. Mirrors the `SourceBuffer.changeType()` extension.
    async fn change_type(&mut self, mime_type: &str) -> Result<(), DashError>;
}

// ── MediaSource trait ─────────────────────────────────────────────────────────

/// Abstraction over the browser MSE `MediaSource` object.
///
/// Consumers create a `MediaSource`, open it, add source buffers per track,
/// append data, and finally call `end_of_stream`. The stub implementation
/// ([`stub::StubMediaSource`]) is fully in-memory.
///
/// Mirrors `ExternalMediaSource.js` from dash.js.
#[async_trait(?Send)]
pub trait MediaSource {
    /// Current ready state.
    fn ready_state(&self) -> ReadyState;

    /// Total presentation duration in seconds, or `None` if unknown.
    fn duration(&self) -> Option<f64>;

    /// Set the total presentation duration. Only valid while `Open`.
    fn set_duration(&mut self, duration: f64) -> Result<(), DashError>;

    /// Create and attach a new [`SourceBuffer`] for the given MIME type.
    /// Only valid while `Open`.
    fn add_source_buffer(&mut self, mime_type: &str) -> Result<Box<dyn SourceBuffer>, DashError>;

    /// Detach and discard the source buffer for the given MIME type.
    fn remove_source_buffer(&mut self, mime_type: &str) -> Result<(), DashError>;

    /// Signal that no more data will be appended.
    /// Transitions the ready state to `Ended`.
    async fn end_of_stream(&mut self) -> Result<(), DashError>;

    /// Close the media source, releasing all resources.
    fn close(&mut self);
}
