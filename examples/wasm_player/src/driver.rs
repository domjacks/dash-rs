//! [`WasmPlaybackDriver`] — [`dash_rs::session::driver::PlaybackDriver`]
//! implementation for the WASM/MSE browser player.
//!
//! Wraps [`JsBridge`] so that [`DashSession::run`] can read renderer
//! position, pause state, and gap-fill seeks from the browser `<video>`
//! element, and push a compact JSON status string to the JavaScript debug
//! panel after every segment pair.

use async_trait::async_trait;
use dash_rs::{
    session::driver::{IterationSummary, PlaybackDriver, PlaybackState},
    GapSeekTarget,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use crate::bridge::JsBridge;

// ── WasmPlaybackDriver ────────────────────────────────────────────────────────

pub(crate) struct WasmPlaybackDriver {
    bridge: JsBridge,
}

impl WasmPlaybackDriver {
    pub(crate) fn new(bridge: &JsBridge) -> Self {
        Self {
            bridge: JsValue::clone(bridge.as_ref()).unchecked_into(),
        }
    }
}

#[async_trait(?Send)]
impl PlaybackDriver for WasmPlaybackDriver {
    fn position_s(&self) -> f64 {
        self.bridge.get_current_time()
    }

    fn playback_state(&self) -> PlaybackState {
        // Buffering is checked first — a stalled renderer is not user-paused.
        if self.bridge.is_buffering() {
            PlaybackState::Buffering
        } else if self.bridge.is_paused() {
            PlaybackState::Paused
        } else {
            PlaybackState::Playing
        }
    }

    async fn on_gap_seek(&mut self, target: GapSeekTarget) {
        self.bridge.seek_to(target.position_s);
    }

    /// Consume any pending user seek posted by the `seeking` event handler in
    /// `app.js`.  Returns `Some(pos)` once then clears the flag; rapid seeks
    /// coalesce to the last position.
    fn pending_seek_s(&mut self) -> Option<f64> {
        let s = self.bridge.get_pending_seek();
        if s.is_nan() {
            None
        } else {
            self.bridge.clear_pending_seek();
            Some(s)
        }
    }

    /// Non-destructive peek — returns `true` while a user seek is pending
    /// without clearing the flag.  Used by the buffer-gate loop to bail out
    /// early so the seek is processed on the next main-loop iteration.
    fn has_pending_seek(&self) -> bool {
        !self.bridge.get_pending_seek().is_nan()
    }

    /// Request PTS remapping so that epoch-scale live timestamps (e.g. BBC
    /// IPTV: ~1.778 × 10⁹ s) are shifted to ≈ 0 for browser MSE.
    fn wants_pts_remap(&self) -> bool {
        true
    }

    /// Seek the `<video>` element to `start_s` (the remapped live-edge
    /// presentation time, ≈ 0 after PTS remap) before the fetch loop starts.
    async fn on_session_started(&mut self, start_s: f64) {
        tracing::info!(start_s, "WasmPlaybackDriver: seeking to session start");
        self.bridge.seek_to(start_s);
    }

    fn on_iteration(&mut self, s: &IterationSummary<'_>) {
        // Build a compact JSON status string and push it to the JS debug panel.
        // All values are primitive scalars — no serde dependency needed.
        let vi = s.video_decision.map_or(0, |d| d.index) as u32;
        let vbps = s.video_bandwidth;
        let ai = s.audio_decision.map_or(0, |d| d.index) as u32;
        let abps = s.audio_bandwidth;

        // Strip characters that would break the JSON string value.
        let url_safe = s.last_transfer_url.replace('"', "").replace('\\', "");

        let json = format!(
            r#"{{"vi":{vi},"vc":{vc},"vbps":{vbps},"vbuf":{vbuf:.1},"ai":{ai},"ac":{ac},"abps":{abps},"abuf":{abuf:.1},"hc":{hc},"sc":{sc},"url":"{url_safe}","tms":{tms:.1},"tbps":{tbps:.0},"live":{live}}}"#,
            vc = s.video_rep_count,
            vbuf = s.video_buffer_ms,
            ac = s.audio_rep_count,
            abuf = s.audio_buffer_ms,
            hc = s.http_request_count,
            sc = s.switch_count,
            tms = s.last_transfer_ms,
            tbps = s.last_throughput_bps.unwrap_or(0.0),
            live = s.is_live,
        );
        self.bridge.report_status(&json);
    }
}
