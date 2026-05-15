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
//! | [`source_buffer`] | `BridgeSourceBuffer` — `SourceBuffer` impl via MSE `SourceBuffer` |
//! | [`media_source`] | `BridgeMediaSource` — `MediaSource` impl via MSE `MediaSource` |
//! | [`fetcher`] | `BridgeFetcher` — `SegmentFetcher` impl via `window.fetch()` |
//! | [`driver`] | `WasmPlaybackDriver` — `PlaybackDriver` impl; status JSON push |
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
//! | MSE | `sourceBuffer.abort()` | `bridge.abortBuffer()` |
//! | MSE | `sourceBuffer.changeType()` | `bridge.changeType()` |
//! | MSE | `sourceBuffer.buffered` | `bridge.getBuffered()` |
//! | MSE | `sourceBuffer.updating` | `bridge.isUpdating()` |
//! | MSE | `sourceBuffer.timestampOffset` | `bridge.getTimestampOffset()` / `setTimestampOffset()` |
//! | MSE | `sourceBuffer.appendWindowStart/End` | `bridge.getAppendWindowStart/End()` / `set*()` |
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
mod media_source;
mod source_buffer;

use url::Url;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use dash_rs::session::{DashSession, MediaType};

use bridge::JsBridge;
use clock::BridgeClock;
use driver::WasmPlaybackDriver;
use fetcher::BridgeFetcher;
use media_source::BridgeMediaSource;

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

    // ── Attach MediaSource to the <video> element ──────────────────────────
    // JS: new MediaSource() → URL.createObjectURL() → video.src= → sourceopen
    JsFuture::from(bridge.setup_media_source())
        .await
        .map_err(|e| JsValue::from_str(&bridge::js_err(e)))?;

    // ── Build the DashSession ──────────────────────────────────────────────

    let url = Url::parse(&mpd_url)
        .map_err(|e| JsValue::from_str(&format!("invalid MPD URL: {e}")))?;

    // Each struct gets its own clone of the bridge (cheap JS reference copy).
    let clock   = BridgeClock::new(JsValue::clone(bridge.as_ref()).unchecked_into());
    let fetcher = BridgeFetcher::new(JsValue::clone(bridge.as_ref()).unchecked_into());
    let ms      = BridgeMediaSource::new(JsValue::clone(bridge.as_ref()).unchecked_into());

    let mut session = DashSession::from_url_with_clock(&url, fetcher, ms, clock)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    tracing::info!(
        has_video = session.has_video(),
        has_audio = session.has_audio(),
        is_live   = session.is_live(),
        "session constructed"
    );

    // ── Live-stream PTS remap + pre-seek ──────────────────────────────────────
    // Safari (and some other browsers) reset video.currentTime to 0 when a seek
    // target is set before content is buffered.  If fMP4 DTS values are
    // epoch-scale (e.g. BBC IPTV: ~1.778 × 10⁹ s), the element stalls forever
    // waiting at position 0 while segments are buffered at ~1.778 × 10⁹ s.
    //
    // Fix: shift timestampOffset on every SourceBuffer sink by
    //   -(initial_segment_number − start_number) × seg_dur_s
    // so that the first live-edge segment lands at presentation time ≈ 0.
    // live_presentation_start_s() then returns ≈ 0, so the seek is trivial.
    //
    // GStreamer clients must NOT call apply_live_pts_remap() — they use
    // GstMediaSource::set_live_mode(true) (probe-based PTS subtraction).
    if session.is_live() {
        bridge
            .set_duration(f64::INFINITY)
            .map_err(|e| JsValue::from_str(&bridge::js_err(e)))?;
        session.apply_live_pts_remap();
        let seek_time = session.live_presentation_start_s();
        bridge.seek_to(seek_time);
        tracing::info!(seek_time, "live stream: PTS remapped; seeked video element to live edge");
    }

    // ── Segment fetch loop ─────────────────────────────────────────────────
    // `run()` handles video_done/audio_done tracking, buffer gate, gap
    // controller, EOS detection, and end_of_stream teardown.  It also calls
    // notify_playing() at startup.
    let mut driver = WasmPlaybackDriver::new(
        &bridge,
        session.representation_count(&MediaType::Video) as u32,
        session.representation_count(&MediaType::Audio) as u32,
        session.is_live(),
    );

    session
        .run(&mut driver)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    tracing::info!("playback complete — end of stream signalled");
    Ok(())
}
