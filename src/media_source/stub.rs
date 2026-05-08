use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{
    error::DashError,
    media_source::{Chunk, MediaSource, ReadyState, SourceBuffer, TimeRange},
};

// ── StubSourceBuffer ──────────────────────────────────────────────────────────

/// In-memory, no-op [`SourceBuffer`] implementation used for testing and
/// offline tooling.
///
/// Mirrors the behaviour of `ExternalSourceBuffer.js` from dash.js:
/// - Stores appended chunks sorted by start time.
/// - Computes [`TimeRange`]s by merging adjacent/overlapping chunks.
/// - Resolves every async call immediately without delay.
pub struct StubSourceBuffer {
    mime_type: String,
    updating: bool,
    chunks: Vec<AppendedChunk>,
    timestamp_offset: f64,
    append_window_start: f64,
    append_window_end: f64,
}

#[derive(Debug, Clone)]
pub struct AppendedChunk {
    data: Bytes,
    start: f64,
    end: f64,
}

impl StubSourceBuffer {
    pub fn new(mime_type: impl Into<String>) -> Self {
        Self {
            mime_type: mime_type.into(),
            updating: false,
            chunks: Vec::new(),
            timestamp_offset: 0.0,
            append_window_start: 0.0,
            append_window_end: f64::INFINITY,
        }
    }

    /// Retrieve a copy of all appended chunks, in chronological order.
    pub fn appended_chunks(&self) -> &[AppendedChunk] {
        &self.chunks
    }

    /// Total bytes appended so far.
    pub fn total_bytes_appended(&self) -> usize {
        self.chunks.iter().map(|c| c.data.len()).sum()
    }

    /// Merge the stored chunks into contiguous [`TimeRange`]s.
    ///
    /// Implements the same algorithm as the `TimeRanges` class inside
    /// `ExternalSourceBuffer.js`: iterate sorted chunks and fold touching or
    /// overlapping ranges.
    fn compute_buffered(&self) -> Vec<TimeRange> {
        let mut ranges: Vec<TimeRange> = Vec::new();

        for chunk in &self.chunks {
            let new = TimeRange::new(chunk.start, chunk.end);
            if let Some(last) = ranges.last_mut() {
                if new.start <= last.end {
                    // Overlapping or adjacent — extend.
                    last.end = last.end.max(new.end);
                    continue;
                }
            }
            ranges.push(new);
        }

        ranges
    }
}

#[async_trait(?Send)]
impl SourceBuffer for StubSourceBuffer {
    fn mime_type(&self) -> &str {
        &self.mime_type
    }

    fn is_updating(&self) -> bool {
        self.updating
    }

    fn buffered(&self) -> Vec<TimeRange> {
        self.compute_buffered()
    }

    fn timestamp_offset(&self) -> f64 {
        self.timestamp_offset
    }

    fn set_timestamp_offset(&mut self, offset: f64) {
        self.timestamp_offset = offset;
    }

    fn append_window_start(&self) -> f64 {
        self.append_window_start
    }

    fn set_append_window_start(&mut self, start: f64) {
        self.append_window_start = start;
    }

    fn append_window_end(&self) -> f64 {
        self.append_window_end
    }

    fn set_append_window_end(&mut self, end: f64) {
        self.append_window_end = end;
    }

    async fn append_buffer(&mut self, chunk: Chunk) -> Result<(), DashError> {
        if self.updating {
            return Err(DashError::SourceBufferUpdating);
        }

        self.updating = true;

        tracing::debug!(
            mime_type = %self.mime_type,
            label = %chunk.label,
            bytes = chunk.data.len(),
            "StubSourceBuffer: appendBuffer"
        );

        if let (Some(start), Some(end)) = (chunk.start, chunk.end) {
            self.chunks.push(AppendedChunk {
                data: chunk.data,
                start,
                end,
            });
            // Keep sorted ascending by start time (mirrors ExternalSourceBuffer.js).
            self.chunks
                .sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
        }
        // Init segments (no timing) are accepted but not tracked in time ranges.

        self.updating = false;
        Ok(())
    }

    async fn remove(&mut self, start: f64, end: f64) -> Result<(), DashError> {
        if self.updating {
            return Err(DashError::SourceBufferUpdating);
        }

        self.updating = true;

        tracing::debug!(
            mime_type = %self.mime_type,
            start,
            end,
            "StubSourceBuffer: remove"
        );

        // Remove chunks that fall entirely within [start, end).
        self.chunks.retain(|c| c.end <= start || c.start >= end);

        self.updating = false;
        Ok(())
    }

    async fn abort(&mut self) -> Result<(), DashError> {
        tracing::debug!(mime_type = %self.mime_type, "StubSourceBuffer: abort");
        self.updating = false;
        Ok(())
    }

    async fn change_type(&mut self, mime_type: &str) -> Result<(), DashError> {
        tracing::debug!(
            old_mime_type = %self.mime_type,
            new_mime_type = mime_type,
            "StubSourceBuffer: changeType"
        );
        self.mime_type = mime_type.to_owned();
        Ok(())
    }
}

// ── StubMediaSource ───────────────────────────────────────────────────────────

/// In-memory, no-op [`MediaSource`] implementation used for testing and
/// offline tooling.
///
/// Mirrors `ExternalMediaSource.js` from dash.js. Emits `tracing::debug!`
/// events for every operation.
pub struct StubMediaSource {
    ready_state: ReadyState,
    duration: Option<f64>,
    /// MIME type → source buffer (we keep track by mime_type as key).
    source_buffers: HashMap<String, ()>,
}

impl Default for StubMediaSource {
    fn default() -> Self {
        Self::new()
    }
}

impl StubMediaSource {
    pub fn new() -> Self {
        Self {
            ready_state: ReadyState::Closed,
            duration: None,
            source_buffers: HashMap::new(),
        }
    }

    /// Open the media source (equivalent to attaching to a `<video>` element
    /// and receiving the `sourceopen` event).
    pub fn open(&mut self) {
        self.ready_state = ReadyState::Open;
        tracing::debug!("StubMediaSource: open");
    }
}

#[async_trait(?Send)]
impl MediaSource for StubMediaSource {
    fn ready_state(&self) -> ReadyState {
        self.ready_state.clone()
    }

    fn duration(&self) -> Option<f64> {
        self.duration
    }

    fn set_duration(&mut self, duration: f64) -> Result<(), DashError> {
        if self.ready_state != ReadyState::Open {
            return Err(DashError::MediaSourceNotOpen {
                state: self.ready_state.clone(),
            });
        }
        tracing::debug!(duration, "StubMediaSource: set duration");
        self.duration = Some(duration);
        Ok(())
    }

    fn add_source_buffer(&mut self, mime_type: &str) -> Result<Box<dyn SourceBuffer>, DashError> {
        if self.ready_state != ReadyState::Open {
            return Err(DashError::MediaSourceNotOpen {
                state: self.ready_state.clone(),
            });
        }
        tracing::debug!(mime_type, "StubMediaSource: addSourceBuffer");
        self.source_buffers.insert(mime_type.to_owned(), ());
        Ok(Box::new(StubSourceBuffer::new(mime_type)))
    }

    fn remove_source_buffer(&mut self, mime_type: &str) -> Result<(), DashError> {
        if self.source_buffers.remove(mime_type).is_none() {
            return Err(DashError::SourceBufferNotFound {
                mime_type: mime_type.to_owned(),
            });
        }
        tracing::debug!(mime_type, "StubMediaSource: removeSourceBuffer");
        Ok(())
    }

    async fn end_of_stream(&mut self) -> Result<(), DashError> {
        if self.ready_state != ReadyState::Open {
            return Err(DashError::MediaSourceNotOpen {
                state: self.ready_state.clone(),
            });
        }
        tracing::debug!("StubMediaSource: endOfStream");
        self.ready_state = ReadyState::Ended;
        Ok(())
    }

    fn close(&mut self) {
        tracing::debug!("StubMediaSource: close");
        self.ready_state = ReadyState::Closed;
        self.source_buffers.clear();
        self.duration = None;
    }
}
