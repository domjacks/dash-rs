//! WASM player example for dash-rs — JS-bridge architecture.
//!
//! All browser Web API calls live in the JavaScript layer (`player.js`).
//! Rust/WASM drives the DASH logic (ABR, manifest parsing, segment scheduling)
//! and calls out to JS for every browser interaction via the [`JsBridge`]
//! extern type defined in [`bridge`].
//!
//! # Module layout
//!
//! | Module | Contents |
//! |---|---|
//! | [`bridge`] | `JsBridge` extern type — every browser Web API method; `js_err` helper |
//! | [`clock`] | `BridgeClock` — `Clock` impl via `performance.now()` / `Date.now()` |
//! | [`media_buffer`] | `BridgeMediaBuffer` — `MediaBuffer` impl via MSE `MediaSource` + `SourceBuffer` |
//! | [`fetcher`] | `BridgeFetcher` — `SegmentFetcher` impl via `window.fetch()` |
//! | [`driver`] | `WasmPlaybackDriver` — `PlaybackDriver` impl for renderer control |
//!
//! # API surface visible in JavaScript
//!
//! | Category | Web API | JS bridge method |
//! |---|---|---|
//! | Clock | `performance.now()` | `bridge.nowMs()` |
//! | Clock | `Date.now()` | `bridge.unixNowS()` |
//! | Utility | `setTimeout()` | `bridge.sleep(ms)` |
//! | Utility | `video.currentTime =` | `bridge.seekTo(time_s)` |
//! | Utility | `video.currentTime` | `bridge.getCurrentTime()` |
//! | MSE | `new MediaSource()` | `bridge.setupMediaSource()` |
//! | MSE | `URL.createObjectURL()` | `bridge.setupMediaSource()` |
//! | MSE | `video.src = objectUrl` | `bridge.setupMediaSource()` |
//! | MSE | `sourceopen` event | `bridge.setupMediaSource()` |
//! | MSE | `mediaSource.readyState` | `bridge.getReadyState()` |
//! | MSE | `mediaSource.duration` | `bridge.getDuration()` / `setDuration()` |
//! | MSE | `mediaSource.addSourceBuffer()` | `bridge.addSourceBuffer()` |
//! | MSE | `mediaSource.removeSourceBuffer()` | `bridge.removeSourceBuffer()` |
//! | MSE | `mediaSource.endOfStream()` | `bridge.endOfStream()` |
//! | MSE | `URL.revokeObjectURL()` | `bridge.close()` |
//! | MSE | `sourceBuffer.appendBuffer()` | `bridge.appendBuffer()` |
//! | MSE | `updateend` event | `bridge.appendBuffer()` |
//! | MSE | `sourceBuffer.remove()` | `bridge.removeRange()` |
//! | MSE | `sourceBuffer.changeType()` | `bridge.changeType()` |
//! | MSE | `sourceBuffer.buffered` | `bridge.getBuffered()` |
//! | MSE | `sourceBuffer.timestampOffset` | `bridge.setTimestampOffset()` |
//! | MSE | `sourceBuffer.appendWindowStart/End` | `bridge.setAppendWindowStart/End()` |
//! | Fetch | `fetch()` | `bridge.fetchBytes()` |
//! | Fetch | `response.arrayBuffer()` | `bridge.fetchBytes()` |
//! | Fetch | `fetch()` HEAD | `bridge.checkExists()` |
//!
//! # Build
//!
//! ```sh
//! wasm-pack build examples/wasm_player --target web --out-dir pkg
//! ```
//!
//! Serve `examples/wasm_player/` with any static file server and open
//! `index.html` in a browser.  The browser DevTools console and the on-screen
//! API log both show every Web API call as it happens.

mod bridge;
mod clock;
mod driver;
mod fetcher;
mod media_buffer;

use url::Url;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use dash_rs::{PlaybackState, Player, PlayerEvent, PlayerIterationEvent, PlayerLogLevel};

use bridge::JsBridge;
use clock::BridgeClock;
use driver::WasmPlaybackDriver;
use fetcher::BridgeFetcher;
use media_buffer::BridgeMediaBuffer;

// ── Status JSON ───────────────────────────────────────────────────────────────

struct WasmPlayerStatus {
    state: &'static str,
    vi: u32,
    vc: u32,
    vbps: u64,
    vbuf: f64,
    ai: u32,
    ac: u32,
    abps: u64,
    abuf: f64,
    hc: usize,
    sc: usize,
    url: String,
    tms: f64,
    tbps: f64,
    live: bool,
    last_event: String,
}

impl WasmPlayerStatus {
    fn new() -> Self {
        Self {
            state: "Starting",
            vi: 0,
            vc: 0,
            vbps: 0,
            vbuf: 0.0,
            ai: 0,
            ac: 0,
            abps: 0,
            abuf: 0.0,
            hc: 0,
            sc: 0,
            url: String::new(),
            tms: 0.0,
            tbps: 0.0,
            live: false,
            last_event: String::new(),
        }
    }

    fn apply_event(&mut self, event: &PlayerEvent) {
        match event {
            PlayerEvent::Iteration(iteration) => self.apply_iteration(iteration),
            PlayerEvent::Log(log) => {
                self.last_event = log.message.clone();
            }
            PlayerEvent::PlaybackStateChanged { to, .. } => {
                self.state = playback_state_label(*to);
            }
            PlayerEvent::QualityChange {
                media_type,
                old_index,
                new_index,
                ..
            } => {
                self.last_event = format!("{media_type:?} quality {old_index} -> {new_index}");
            }
            PlayerEvent::GapSeek(target) => {
                self.last_event = format!("gap seek -> {:.2}s", target.position_s);
            }
            PlayerEvent::SessionStarted { start_s } => {
                self.state = "Playing";
                self.last_event = format!("session started @ {start_s:.2}s");
            }
            PlayerEvent::InbandDash(ev) => {
                self.last_event = format!("emsg {} id {}", ev.scheme_id_uri, ev.id);
            }
            PlayerEvent::EndOfStream => {
                self.state = "Ended";
                self.last_event = "end of stream".to_owned();
            }
        }
    }

    fn apply_iteration(&mut self, s: &PlayerIterationEvent) {
        self.vi = s.video_index.unwrap_or(0) as u32;
        self.vc = s.video_rep_count;
        self.vbps = s.video_bandwidth;
        self.vbuf = s.video_buffer_ms;
        self.ai = s.audio_index.unwrap_or(0) as u32;
        self.ac = s.audio_rep_count;
        self.abps = s.audio_bandwidth;
        self.abuf = s.audio_buffer_ms;
        self.hc = s.http_request_count;
        self.sc = s.switch_count;
        self.url = s.last_transfer_url.clone();
        self.tms = s.last_transfer_ms;
        self.tbps = s.last_throughput_bps.unwrap_or(0.0);
        self.live = s.is_live;
    }

    fn to_json(&self) -> String {
        let url_safe = json_string_fragment(&self.url);
        let event_safe = json_string_fragment(&self.last_event);
        format!(
            r#"{{"state":"{state}","vi":{vi},"vc":{vc},"vbps":{vbps},"vbuf":{vbuf:.1},"ai":{ai},"ac":{ac},"abps":{abps},"abuf":{abuf:.1},"hc":{hc},"sc":{sc},"url":"{url_safe}","tms":{tms:.1},"tbps":{tbps:.0},"live":{live},"event":"{event_safe}"}}"#,
            state = self.state,
            vi = self.vi,
            vc = self.vc,
            vbps = self.vbps,
            vbuf = self.vbuf,
            ai = self.ai,
            ac = self.ac,
            abps = self.abps,
            abuf = self.abuf,
            hc = self.hc,
            sc = self.sc,
            tms = self.tms,
            tbps = self.tbps,
            live = self.live,
        )
    }
}

fn playback_state_label(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Playing => "Playing",
        PlaybackState::Paused => "Paused",
        PlaybackState::Buffering => "Buffering",
    }
}

fn json_string_fragment(s: &str) -> String {
    s.replace(['\\', '"'], "")
}

fn log_level_label(level: PlayerLogLevel) -> &'static str {
    match level {
        PlayerLogLevel::Debug => "debug",
        PlayerLogLevel::Info => "info",
        PlayerLogLevel::Warn => "warn",
        PlayerLogLevel::Error => "error",
    }
}

// ── WASM init ─────────────────────────────────────────────────────────────────

/// Initialise WASM infrastructure (panic hook + tracing).
///
/// Called automatically by the WASM module initialiser before any exported
/// function runs.
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();
}

// ── play ──────────────────────────────────────────────────────────────────────

/// Start MPEG-DASH playback.
///
/// # Arguments
///
/// * `mpd_url` — fully qualified URL to the `.mpd` manifest.
/// * `bridge`  — JS object that implements every browser Web API that dash-rs
///   needs.  See `player.js` for the full annotated implementation.
///
/// # Usage from JavaScript
///
/// ```js
/// import init, { play } from './pkg/wasm_player.js';
/// await init();
/// await play('https://example.com/manifest.mpd', bridge);
/// ```
#[wasm_bindgen]
pub async fn play(mpd_url: String, bridge: JsBridge) -> Result<(), JsValue> {
    tracing::info!(mpd_url = %mpd_url, "dash-rs WASM player starting");

    let url =
        Url::parse(&mpd_url).map_err(|e| JsValue::from_str(&format!("invalid MPD URL: {e}")))?;

    // Each platform type gets its own clone of the bridge (cheap JS ref copy).
    let clock = BridgeClock::new(JsValue::clone(bridge.as_ref()).unchecked_into());
    let fetcher = BridgeFetcher::new(JsValue::clone(bridge.as_ref()).unchecked_into());
    let buf = BridgeMediaBuffer::new(JsValue::clone(bridge.as_ref()).unchecked_into());
    let driver = WasmPlaybackDriver::new(&bridge);

    // `Player::from_url_with_clock` fetches + parses the manifest and returns a
    // Player ready to run.  `run()` then handles the full startup sequence:
    //   set_live_mode → set_duration(+∞) → add_track × 2 →
    //   apply_live_pts_remap (wants_pts_remap=true) →
    //   on_session_started (seekTo live-edge ≈ 0) → notify_playing → loop.
    let mut player = Player::from_url_with_clock(&url, fetcher, buf, driver, clock)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    let status_bridge: JsBridge = JsValue::clone(bridge.as_ref()).unchecked_into();
    let mut status = WasmPlayerStatus::new();
    status_bridge.report_status(&status.to_json());
    player.on(move |event| {
        if let PlayerEvent::Log(log) = event {
            status_bridge.report_log(log_level_label(log.level), log.target, &log.message);
        }
        status.apply_event(event);
        status_bridge.report_status(&status.to_json());
    });

    player
        .run()
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    tracing::info!("playback complete — end of stream signalled");
    Ok(())
}
