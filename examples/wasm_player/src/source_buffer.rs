//! [`BridgeSourceBuffer`] — [`dash_rs::media_source::SourceBuffer`]
//! implementation that delegates every operation to the [`JsBridge`].
//!
//! `sb_id` is the opaque identifier returned by `bridge.addSourceBuffer()`.
//! The JS side uses it to look up the corresponding browser `SourceBuffer`
//! object.
//!
//! Append and remove operations return Promises in JS (resolved on the
//! `updateend` event), so they are event-driven rather than polled.

use async_trait::async_trait;
use dash_rs::{
    media_source::{Chunk, SourceBuffer, TimeRange},
    DashError,
};
use js_sys::Uint8Array;
use wasm_bindgen_futures::JsFuture;

use crate::bridge::{js_err, JsBridge};

// ── BridgeSourceBuffer ────────────────────────────────────────────────────────

pub(crate) struct BridgeSourceBuffer {
    bridge:    JsBridge,
    sb_id:     String,
    mime_type: String,
}

impl BridgeSourceBuffer {
    pub(crate) fn new(bridge: JsBridge, sb_id: String, mime_type: String) -> Self {
        Self { bridge, sb_id, mime_type }
    }
}

#[async_trait(?Send)]
impl SourceBuffer for BridgeSourceBuffer {
    fn mime_type(&self) -> &str {
        &self.mime_type
    }

    fn is_updating(&self) -> bool {
        self.bridge.is_updating(&self.sb_id)
    }

    fn buffered(&self) -> Vec<TimeRange> {
        // JS returns a flat Float64Array: [start0, end0, start1, end1, …]
        self.bridge
            .get_buffered(&self.sb_id)
            .to_vec()
            .chunks_exact(2)
            .map(|c| TimeRange::new(c[0], c[1]))
            .collect()
    }

    fn timestamp_offset(&self) -> f64 {
        self.bridge.get_timestamp_offset(&self.sb_id)
    }

    fn set_timestamp_offset(&mut self, offset: f64) {
        self.bridge.set_timestamp_offset(&self.sb_id, offset);
    }

    fn append_window_start(&self) -> f64 {
        self.bridge.get_append_window_start(&self.sb_id)
    }

    fn set_append_window_start(&mut self, start: f64) {
        self.bridge.set_append_window_start(&self.sb_id, start);
    }

    fn append_window_end(&self) -> f64 {
        self.bridge.get_append_window_end(&self.sb_id)
    }

    fn set_append_window_end(&mut self, end: f64) {
        self.bridge.set_append_window_end(&self.sb_id, end);
    }

    async fn append_buffer(&mut self, chunk: Chunk) -> Result<(), DashError> {
        tracing::debug!(
            mime_type = %self.mime_type,
            label     = %chunk.label,
            bytes     = chunk.data.len(),
            "BridgeSourceBuffer: appendBuffer"
        );
        let array = Uint8Array::from(chunk.data.as_ref());
        JsFuture::from(self.bridge.append_buffer(&self.sb_id, &array))
            .await
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        Ok(())
    }

    async fn remove(&mut self, start: f64, end: f64) -> Result<(), DashError> {
        tracing::debug!(
            mime_type = %self.mime_type,
            start,
            end,
            "BridgeSourceBuffer: remove"
        );
        JsFuture::from(self.bridge.remove_range(&self.sb_id, start, end))
            .await
            .map_err(|e| DashError::RemoveFailed(js_err(e)))?;
        Ok(())
    }

    async fn abort(&mut self) -> Result<(), DashError> {
        tracing::debug!(mime_type = %self.mime_type, "BridgeSourceBuffer: abort");
        self.bridge
            .abort_buffer(&self.sb_id)
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        Ok(())
    }

    async fn change_type(&mut self, mime_type: &str) -> Result<(), DashError> {
        tracing::debug!(
            old = %self.mime_type,
            new = mime_type,
            "BridgeSourceBuffer: changeType"
        );
        self.bridge
            .change_type(&self.sb_id, mime_type)
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        self.mime_type = mime_type.to_owned();
        Ok(())
    }
}
