//! [`BridgeMediaBuffer`] — [`dash_rs::media_buffer::MediaBuffer`]
//! implementation that delegates every operation to the [`JsBridge`].
//!
//! `video_sb_id` / `audio_sb_id` hold the opaque source-buffer IDs returned
//! by `bridge.addSourceBuffer()`, so that per-track operations such as
//! `append`, `remove_range`, and `set_timestamp_offset` can address the
//! correct browser `SourceBuffer` object.

use async_trait::async_trait;
use bytes::Bytes;
use dash_rs::{
    error::DashError,
    media_buffer::{MediaBuffer, TimeRange},
    session::MediaType,
};
use js_sys::Uint8Array;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use crate::bridge::{js_err, JsBridge};

// ── BridgeMediaBuffer ─────────────────────────────────────────────────────────

pub(crate) struct BridgeMediaBuffer {
    bridge: JsBridge,
    /// Opaque SourceBuffer ID for the video track (set by `add_track`).
    video_sb_id: Option<String>,
    /// Opaque SourceBuffer ID for the audio track (set by `add_track`).
    audio_sb_id: Option<String>,
}

impl BridgeMediaBuffer {
    pub(crate) fn new(bridge: JsBridge) -> Self {
        Self {
            bridge,
            video_sb_id: None,
            audio_sb_id: None,
        }
    }

    /// Return the SourceBuffer ID for `mt`, or an error if the track has not
    /// been initialised via [`add_track`].
    fn sb_id(&self, mt: MediaType) -> Result<&str, DashError> {
        let opt = match mt {
            MediaType::Video => &self.video_sb_id,
            MediaType::Audio => &self.audio_sb_id,
        };
        opt.as_deref()
            .ok_or_else(|| DashError::AppendFailed(format!("{mt:?} track not initialised")))
    }
}

#[async_trait(?Send)]
impl MediaBuffer for BridgeMediaBuffer {
    /// Attach the `MediaSource` to the `<video>` element and wait for the
    /// `sourceopen` event.  Called automatically by
    /// `DashSession::from_url_with_clock` before `add_track`.
    async fn open(&mut self) -> Result<(), DashError> {
        tracing::debug!("BridgeMediaBuffer: open — awaiting sourceopen");
        JsFuture::from(self.bridge.setup_media_source())
            .await
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        tracing::info!("BridgeMediaBuffer: sourceopen received — MediaSource is Open");
        Ok(())
    }

    fn set_duration(&mut self, secs: f64) {
        let _ = self.bridge.set_duration(secs);
    }

    async fn end_of_stream(&mut self) -> Result<(), DashError> {
        tracing::info!("BridgeMediaBuffer: endOfStream");
        self.bridge
            .end_of_stream()
            .map_err(|e| DashError::AppendFailed(js_err(e)))
    }

    /// Allocate a browser `SourceBuffer` for `mt`.
    ///
    /// Calls `MediaSource.isTypeSupported(mime_type)` first; returns `false`
    /// to skip the track if the browser does not support the codec (e.g. HEVC
    /// on Chrome/Firefox).  Otherwise calls `addSourceBuffer(mime_type)` and
    /// stores the returned ID.
    fn add_track(&mut self, mt: MediaType, mime_type: &str) -> bool {
        let supported = self.bridge.is_type_supported(mime_type);
        if !supported {
            tracing::warn!(
                mime_type,
                "MediaSource.isTypeSupported() returned false — \
                 track will be skipped (HEVC requires Safari on macOS/iOS)"
            );
            return false;
        }

        tracing::debug!(mime_type, ?mt, "BridgeMediaBuffer: addSourceBuffer");
        match self.bridge.add_source_buffer(mime_type) {
            Ok(id) => {
                match mt {
                    MediaType::Video => self.video_sb_id = Some(id),
                    MediaType::Audio => self.audio_sb_id = Some(id),
                }
                true
            }
            Err(e) => {
                tracing::error!(
                    mime_type,
                    err = %js_err(e),
                    "BridgeMediaBuffer: addSourceBuffer failed"
                );
                false
            }
        }
    }

    async fn change_type(&mut self, mt: MediaType, mime_type: &str) -> Result<(), DashError> {
        let id = self.sb_id(mt)?.to_owned();
        tracing::debug!(?mt, new = mime_type, "BridgeMediaBuffer: changeType");
        self.bridge
            .change_type(&id, mime_type)
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        // Update the stored ID in case the browser re-keyed it (rare but safe).
        Ok(())
    }

    fn set_timestamp_offset(&mut self, mt: MediaType, offset_s: f64) {
        if let Ok(id) = self.sb_id(mt) {
            let id = id.to_owned();
            self.bridge.set_timestamp_offset(&id, offset_s);
        }
    }

    fn set_append_window(&mut self, mt: MediaType, start: f64, end: f64) {
        if let Ok(id) = self.sb_id(mt) {
            let id = id.to_owned();
            self.bridge.set_append_window_start(&id, start);
            self.bridge.set_append_window_end(&id, end);
        }
    }

    async fn append(
        &mut self,
        mt: MediaType,
        data: Bytes,
        _is_init: bool,
    ) -> Result<(), DashError> {
        let id = self.sb_id(mt)?.to_owned();
        tracing::debug!(?mt, bytes = data.len(), "BridgeMediaBuffer: appendBuffer");
        let array = Uint8Array::from(data.as_ref());
        JsFuture::from(self.bridge.append_buffer(&id, &array))
            .await
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        Ok(())
    }

    async fn remove_range(&mut self, mt: MediaType, start: f64, end: f64) -> Result<(), DashError> {
        let id = self.sb_id(mt)?.to_owned();
        tracing::debug!(?mt, start, end, "BridgeMediaBuffer: remove");
        JsFuture::from(self.bridge.remove_range(&id, start, end))
            .await
            .map_err(|e| DashError::RemoveFailed(js_err(e)))?;
        Ok(())
    }

    /// Read `SourceBuffer.buffered` from the browser and convert to
    /// [`TimeRange`] pairs.  The JS bridge returns a flat `Float64Array`
    /// `[start0, end0, start1, end1, …]`.
    fn buffered(&self, mt: MediaType) -> Vec<TimeRange> {
        let Ok(id) = self.sb_id(mt) else {
            return vec![];
        };
        self.bridge
            .get_buffered(id)
            .to_vec()
            .chunks_exact(2)
            .map(|c| TimeRange::new(c[0], c[1]))
            .collect()
    }
}
