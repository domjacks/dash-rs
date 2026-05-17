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

use dash_rs::Player;

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

    let url =
        Url::parse(&mpd_url).map_err(|e| JsValue::from_str(&format!("invalid MPD URL: {e}")))?;

    // Each platform type gets its own clone of the bridge (cheap JS ref copy).
    let clock = BridgeClock::new(JsValue::clone(bridge.as_ref()).unchecked_into());
    let fetcher = BridgeFetcher::new(JsValue::clone(bridge.as_ref()).unchecked_into());
    let ms = BridgeMediaSource::new(JsValue::clone(bridge.as_ref()).unchecked_into());
    let driver = WasmPlaybackDriver::new(&bridge);

    // `Player::from_url_with_clock` fetches + parses the manifest, opens the
    // media source (awaiting the `sourceopen` event), and returns a Player
    // ready to run.  `run()` then handles the full startup sequence:
    //   set_live_mode → set_duration(+∞) → fetch_init_segments →
    //   apply_live_pts_remap (wants_pts_remap=true) →
    //   on_session_started (seekTo live-edge ≈ 0) → notify_playing → loop.
    Player::from_url_with_clock(&url, fetcher, ms, driver, clock)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?
        .run()
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    tracing::info!("playback complete — end of stream signalled");
    Ok(())
}
