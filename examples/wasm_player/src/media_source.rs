//! [`BridgeMediaSource`] — [`dash_rs::media_source::MediaSource`]
//! implementation that delegates every operation to the [`JsBridge`].
//!
//! `mime_to_id` maps a MIME type string (e.g. `"video/mp4;codecs=avc1…"`) to
//! the opaque source-buffer ID assigned by `bridge.addSourceBuffer()`, so that
//! `remove_source_buffer` can find the right browser object by MIME type.

use std::collections::HashMap;

use async_trait::async_trait;
use dash_rs::{
    media_source::{MediaSource, ReadyState, SourceBuffer},
    DashError,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use crate::bridge::{js_err, JsBridge};
use crate::source_buffer::BridgeSourceBuffer;

// ── BridgeMediaSource ─────────────────────────────────────────────────────────

pub(crate) struct BridgeMediaSource {
    bridge: JsBridge,
    mime_to_id: HashMap<String, String>,
}

impl BridgeMediaSource {
    pub(crate) fn new(bridge: JsBridge) -> Self {
        Self {
            bridge,
            mime_to_id: HashMap::new(),
        }
    }
}

#[async_trait(?Send)]
impl MediaSource for BridgeMediaSource {
    /// Attach the `MediaSource` to the `<video>` element and wait for the
    /// `sourceopen` event.  Called automatically by
    /// `DashSession::from_url_with_clock` before `add_source_buffer`.
    async fn open(&mut self) -> Result<(), DashError> {
        tracing::debug!("BridgeMediaSource: open — awaiting sourceopen");
        JsFuture::from(self.bridge.setup_media_source())
            .await
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        tracing::info!("BridgeMediaSource: sourceopen received — MediaSource is Open");
        Ok(())
    }

    fn ready_state(&self) -> ReadyState {
        match self.bridge.get_ready_state().as_str() {
            "open" => ReadyState::Open,
            "ended" => ReadyState::Ended,
            _ => ReadyState::Closed,
        }
    }

    fn duration(&self) -> Option<f64> {
        let d = self.bridge.get_duration();
        if d.is_nan() || d < 0.0 {
            None
        } else {
            Some(d)
        }
    }

    fn set_duration(&mut self, duration: f64) -> Result<(), DashError> {
        self.bridge
            .set_duration(duration)
            .map_err(|e| DashError::AppendFailed(js_err(e)))
    }

    fn is_type_supported(&self, mime_type: &str) -> bool {
        let supported = self.bridge.is_type_supported(mime_type);
        if !supported {
            tracing::warn!(
                mime_type,
                "MediaSource.isTypeSupported() returned false — \
                 track will be skipped (HEVC requires Safari on macOS/iOS)"
            );
        }
        supported
    }

    fn add_source_buffer(&mut self, mime_type: &str) -> Result<Box<dyn SourceBuffer>, DashError> {
        tracing::debug!(mime_type, "BridgeMediaSource: addSourceBuffer");
        let sb_id = self
            .bridge
            .add_source_buffer(mime_type)
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        self.mime_to_id.insert(mime_type.to_owned(), sb_id.clone());
        Ok(Box::new(BridgeSourceBuffer::new(
            JsValue::clone(self.bridge.as_ref()).unchecked_into(),
            sb_id,
            mime_type.to_owned(),
        )))
    }

    fn remove_source_buffer(&mut self, mime_type: &str) -> Result<(), DashError> {
        let sb_id =
            self.mime_to_id
                .remove(mime_type)
                .ok_or_else(|| DashError::SourceBufferNotFound {
                    mime_type: mime_type.to_owned(),
                })?;
        self.bridge
            .remove_source_buffer_by_id(&sb_id)
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        tracing::debug!(mime_type, "BridgeMediaSource: removeSourceBuffer");
        Ok(())
    }

    async fn end_of_stream(&mut self) -> Result<(), DashError> {
        tracing::info!("BridgeMediaSource: endOfStream");
        self.bridge
            .end_of_stream()
            .map_err(|e| DashError::AppendFailed(js_err(e)))
    }

    fn close(&mut self) {
        tracing::info!("BridgeMediaSource: close — revoking object URL");
        self.bridge.close_bridge();
    }
}
