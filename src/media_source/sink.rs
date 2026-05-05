use crate::{
    error::DashError,
    media_source::{Chunk, SourceBuffer, TimeRange},
};

/// The time (seconds) by which `appendWindowStart` is expanded inward to
/// account for imprecise segment boundaries. Mirrors
/// `APPEND_WINDOW_START_OFFSET` in `SourceBufferSink.js`.
const APPEND_WINDOW_START_OFFSET: f64 = 0.1;
/// The time (seconds) by which `appendWindowEnd` is expanded outward.
/// Mirrors `APPEND_WINDOW_END_OFFSET` in `SourceBufferSink.js`.
const APPEND_WINDOW_END_OFFSET: f64 = 0.01;

/// Information about a stream period used to configure the append window.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    /// Period start time in seconds.
    pub start: f64,
    /// Period duration in seconds, or `None` for open-ended/live.
    pub duration: Option<f64>,
}

/// Queuing wrapper around a [`SourceBuffer`].
///
/// Mirrors `SourceBufferSink.js` from dash.js. Responsibilities:
///
/// - Serialises concurrent appends (the underlying MSE `SourceBuffer` only
///   allows one append at a time).
/// - Manages `appendWindowStart` / `appendWindowEnd` relative to stream
///   period boundaries.
/// - Records the label of the last appended chunk for error reporting.
///
/// In dash.js these concerns are handled by the Promise-based queue and
/// the `_waitForUpdateEnd` helper. Here, Rust's `async/await` provides
/// natural serialisation — callers simply `await` each `append()` call.
pub struct SourceBufferSink {
    inner: Box<dyn SourceBuffer>,
    last_appended_label: Option<String>,
}

impl SourceBufferSink {
    /// Wrap a concrete [`SourceBuffer`] implementation.
    pub fn new(inner: Box<dyn SourceBuffer>) -> Self {
        Self {
            inner,
            last_appended_label: None,
        }
    }

    /// The MIME type of the underlying buffer.
    pub fn mime_type(&self) -> &str {
        self.inner.mime_type()
    }

    /// Buffered time ranges.
    pub fn buffered(&self) -> Vec<TimeRange> {
        self.inner.buffered()
    }

    /// Label of the last successfully appended chunk.
    pub fn last_appended_label(&self) -> Option<&str> {
        self.last_appended_label.as_deref()
    }

    /// Configure the append window from a [`StreamInfo`].
    ///
    /// Mirrors `SourceBufferSink.updateAppendWindow()`.
    pub fn update_append_window(
        &mut self,
        stream_info: &StreamInfo,
        media_source_duration: Option<f64>,
    ) {
        let append_window_end = if let Some(dur) = stream_info.duration {
            if dur.is_finite() {
                stream_info.start + dur
            } else {
                media_source_duration.unwrap_or(f64::INFINITY)
            }
        } else {
            media_source_duration.unwrap_or(f64::INFINITY)
        };

        let append_window_start = (stream_info.start - APPEND_WINDOW_START_OFFSET).max(0.0);
        let append_window_end = append_window_end + APPEND_WINDOW_END_OFFSET;

        self.inner.set_append_window_start(0.0); // Reset before setting end to avoid constraint violation.
        self.inner.set_append_window_end(append_window_end);
        self.inner.set_append_window_start(append_window_start);

        tracing::debug!(
            mime_type = self.inner.mime_type(),
            append_window_start,
            append_window_end,
            "updated append window"
        );
    }

    /// Set the MSE timestamp offset.
    ///
    /// Mirrors `SourceBufferSink.updateTimestampOffset()`.
    pub fn set_timestamp_offset(&mut self, offset: f64) {
        if self.inner.timestamp_offset() != offset && offset.is_finite() {
            self.inner.set_timestamp_offset(offset);
            tracing::debug!(
                mime_type = self.inner.mime_type(),
                offset,
                "set timestamp offset"
            );
        }
    }

    /// Append a chunk of media data to the underlying buffer.
    ///
    /// Awaiting this call serialises naturally — the next `append()` call
    /// will not begin until this one resolves, provided the caller `await`s
    /// each call in sequence (or uses a single-consumer queue).
    pub async fn append(&mut self, chunk: Chunk) -> Result<(), DashError> {
        if self.inner.is_updating() {
            return Err(DashError::SourceBufferUpdating);
        }

        let label = chunk.label.clone();

        tracing::debug!(
            mime_type = self.inner.mime_type(),
            label,
            bytes = chunk.data.len(),
            start = ?chunk.start,
            end   = ?chunk.end,
            "appending chunk"
        );

        self.inner.append_buffer(chunk).await?;
        self.last_appended_label = Some(label);
        Ok(())
    }

    /// Abort an in-progress append and reset parser state.
    ///
    /// Mirrors `SourceBufferSink.abortBeforeAppend()`.
    pub async fn abort(&mut self) -> Result<(), DashError> {
        // Save append window before abort (MSE resets it on abort()).
        let aws = self.inner.append_window_start();
        let awe = self.inner.append_window_end();

        self.inner.abort().await?;

        self.inner.set_append_window_start(0.0);
        self.inner.set_append_window_end(awe);
        self.inner.set_append_window_start(aws);

        Ok(())
    }

    /// Remove buffered data in the time range `[start, end)`.
    ///
    /// Mirrors `SourceBufferSink.remove()`.
    pub async fn remove(&mut self, start: f64, end: f64) -> Result<(), DashError> {
        if !(start >= 0.0 && end > start) {
            return Err(DashError::InvalidTimeRange { start, end });
        }

        tracing::debug!(
            mime_type = self.inner.mime_type(),
            start,
            end,
            "removing buffered range"
        );

        self.inner.remove(start, end).await
    }
}
