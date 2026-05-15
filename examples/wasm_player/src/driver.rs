//! [`WasmPlaybackDriver`] — [`dash_rs::session::driver::PlaybackDriver`]
//! implementation for the WASM/MSE browser player.
//!
//! Wraps [`JsBridge`] so that [`DashSession::run`] can read renderer
//! position, pause state, and gap-fill seeks from the browser `<video>`
//! element, and push a compact JSON status string to the JavaScript debug
//! panel after every segment pair.

use async_trait::async_trait;
use dash_rs::{
    session::driver::{IterationSummary, PlaybackDriver},
    GapSeekTarget,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

use crate::bridge::JsBridge;

// ── WasmPlaybackDriver ────────────────────────────────────────────────────────

pub(crate) struct WasmPlaybackDriver {
    bridge: JsBridge,
    /// Cached video representation count (for the status JSON).
    video_rep_count: u32,
    /// Cached audio representation count (for the status JSON).
    audio_rep_count: u32,
    /// Whether the session is a live stream (for the status JSON).
    is_live: bool,
}

impl WasmPlaybackDriver {
    pub(crate) fn new(
        bridge: &JsBridge,
        video_rep_count: u32,
        audio_rep_count: u32,
        is_live: bool,
    ) -> Self {
        Self {
            bridge: JsValue::clone(bridge.as_ref()).unchecked_into(),
            video_rep_count,
            audio_rep_count,
            is_live,
        }
    }
}

#[async_trait(?Send)]
impl PlaybackDriver for WasmPlaybackDriver {
    fn position_s(&self) -> f64 {
        self.bridge.get_current_time()
    }

    fn is_paused(&self) -> bool {
        self.bridge.is_paused()
    }

    async fn on_gap_seek(&mut self, target: GapSeekTarget) {
        self.bridge.seek_to(target.position_s);
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
            vc = self.video_rep_count,
            vbuf = s.video_buffer_ms,
            ac = self.audio_rep_count,
            abuf = s.audio_buffer_ms,
            hc = s.http_request_count,
            sc = s.switch_count,
            tms = s.last_transfer_ms,
            tbps = s.last_throughput_bps,
            live = self.is_live,
        );
        self.bridge.report_status(&json);
    }
}
