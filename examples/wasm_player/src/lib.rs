//! WASM player example for dash-rs — JS-bridge architecture.
//!
//! All browser Web API calls live in the JavaScript layer (`index.html`).
//! Rust/WASM drives the DASH logic (ABR, manifest parsing, segment scheduling)
//! and calls out to JS for every browser interaction via the [`JsBridge`]
//! extern type.
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

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use js_sys::{Float64Array, Promise, Uint8Array};
use url::Url;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use dash_rs::{
    clock::Clock,
    fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher},
    media_source::{Chunk, MediaSource, ReadyState, SourceBuffer, TimeRange},
    session::{DashSession, MediaType},
    DashError,
};

// ── JS error helper ───────────────────────────────────────────────────────────

/// Format a `JsValue` (which may be a `DOMException`, `Error`, or plain value)
/// into a human-readable string.
fn js_err(val: JsValue) -> String {
    if let Some(s) = val.as_string() {
        return s;
    }
    let name = js_sys::Reflect::get(&val, &JsValue::from_str("name"))
        .ok()
        .and_then(|v| v.as_string());
    let message = js_sys::Reflect::get(&val, &JsValue::from_str("message"))
        .ok()
        .and_then(|v| v.as_string());
    match (name, message) {
        (Some(n), Some(m)) if !m.is_empty() => format!("{n}: {m}"),
        (Some(n), _) if !n.is_empty()       => n,
        (_, Some(m)) if !m.is_empty()        => m,
        _ => js_sys::JSON::stringify(&val)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| "(opaque JS error)".to_owned()),
    }
}

// ── JsBridge extern type ──────────────────────────────────────────────────────

/// The JavaScript bridge object passed from `index.html`.
///
/// Every browser Web API call that dash-rs needs goes through one of these
/// methods.  The JavaScript implementation of each method is the educational
/// reference — open `index.html` to read the exact Web API calls.
///
/// Methods that can throw synchronously use `#[wasm_bindgen(catch)]` so that
/// JavaScript exceptions surface as `Result::Err` in Rust rather than panics.
/// Methods that return a `Promise` surface errors via the rejected branch of
/// [`JsFuture`].
#[wasm_bindgen]
extern "C" {
    pub type JsBridge;

    // ── Clock ─────────────────────────────────────────────────────────────
    // performance.now() — monotonic milliseconds since navigation start.
    #[wasm_bindgen(method, js_name = nowMs)]
    fn now_ms(this: &JsBridge) -> f64;

    // Date.now() / 1000 — seconds since Unix epoch.
    #[wasm_bindgen(method, js_name = unixNowS)]
    fn unix_now_s(this: &JsBridge) -> f64;

    // ── Utility ───────────────────────────────────────────────────────────
    // new Promise(r => setTimeout(r, ms))
    #[wasm_bindgen(method)]
    fn sleep(this: &JsBridge, ms: f64) -> Promise;

    // video.currentTime = time_s  — seek the media element to the live edge
    // presentation position before any segments are appended.
    #[wasm_bindgen(method, js_name = seekTo)]
    fn seek_to(this: &JsBridge, time_s: f64);

    // video.currentTime — current playback position in seconds.
    // Passed to DashSession::current_metrics() so ABR buffer-level calculations
    // use the real position rather than 0.0 (which would produce enormous
    // buffer_level_ms values for epoch-scale live streams like BBC IPTV).
    #[wasm_bindgen(method, js_name = getCurrentTime)]
    fn get_current_time(this: &JsBridge) -> f64;

    // ── MediaSource lifecycle ─────────────────────────────────────────────
    // new MediaSource() + URL.createObjectURL() + video.src= + sourceopen
    #[wasm_bindgen(method, js_name = setupMediaSource)]
    fn setup_media_source(this: &JsBridge) -> Promise;

    // MediaSource.isTypeSupported(mimeType) — static capability check.
    #[wasm_bindgen(method, js_name = isTypeSupported)]
    fn is_type_supported(this: &JsBridge, mime_type: &str) -> bool;

    // mediaSource.readyState  →  "open" | "ended" | "closed"
    #[wasm_bindgen(method, js_name = getReadyState)]
    fn get_ready_state(this: &JsBridge) -> String;

    // mediaSource.duration
    #[wasm_bindgen(method, js_name = getDuration)]
    fn get_duration(this: &JsBridge) -> f64;

    // mediaSource.duration = d
    #[wasm_bindgen(method, catch, js_name = setDuration)]
    fn set_duration(this: &JsBridge, d: f64) -> Result<(), JsValue>;

    // mediaSource.addSourceBuffer(mimeType)  →  opaque buffer id
    #[wasm_bindgen(method, catch, js_name = addSourceBuffer)]
    fn add_source_buffer(this: &JsBridge, mime_type: &str) -> Result<String, JsValue>;

    // mediaSource.removeSourceBuffer(sourceBuffers[id])
    #[wasm_bindgen(method, catch, js_name = removeSourceBuffer)]
    fn remove_source_buffer_by_id(this: &JsBridge, sb_id: &str) -> Result<(), JsValue>;

    // mediaSource.endOfStream()
    #[wasm_bindgen(method, catch, js_name = endOfStream)]
    fn end_of_stream(this: &JsBridge) -> Result<(), JsValue>;

    // URL.revokeObjectURL(objectUrl)
    #[wasm_bindgen(method, js_name = close)]
    fn close_bridge(this: &JsBridge);

    // ── SourceBuffer operations ────────────────────────────────────────────
    // sb.appendBuffer(data)  +  updateend event  →  Promise<void>
    #[wasm_bindgen(method, js_name = appendBuffer)]
    fn append_buffer(this: &JsBridge, sb_id: &str, data: &Uint8Array) -> Promise;

    // sb.remove(start, end)  +  updateend event  →  Promise<void>
    #[wasm_bindgen(method, js_name = removeRange)]
    fn remove_range(this: &JsBridge, sb_id: &str, start: f64, end: f64) -> Promise;

    // sb.abort()
    #[wasm_bindgen(method, catch, js_name = abortBuffer)]
    fn abort_buffer(this: &JsBridge, sb_id: &str) -> Result<(), JsValue>;

    // sb.changeType(mimeType)
    #[wasm_bindgen(method, catch, js_name = changeType)]
    fn change_type(this: &JsBridge, sb_id: &str, mime_type: &str) -> Result<(), JsValue>;

    // sb.buffered  →  flat Float64Array [start0, end0, start1, end1, …]
    #[wasm_bindgen(method, js_name = getBuffered)]
    fn get_buffered(this: &JsBridge, sb_id: &str) -> Float64Array;

    // sb.updating
    #[wasm_bindgen(method, js_name = isUpdating)]
    fn is_updating(this: &JsBridge, sb_id: &str) -> bool;

    // sb.timestampOffset
    #[wasm_bindgen(method, js_name = getTimestampOffset)]
    fn get_timestamp_offset(this: &JsBridge, sb_id: &str) -> f64;

    #[wasm_bindgen(method, js_name = setTimestampOffset)]
    fn set_timestamp_offset(this: &JsBridge, sb_id: &str, offset: f64);

    // sb.appendWindowStart
    #[wasm_bindgen(method, js_name = getAppendWindowStart)]
    fn get_append_window_start(this: &JsBridge, sb_id: &str) -> f64;

    #[wasm_bindgen(method, js_name = setAppendWindowStart)]
    fn set_append_window_start(this: &JsBridge, sb_id: &str, start: f64);

    // sb.appendWindowEnd
    #[wasm_bindgen(method, js_name = getAppendWindowEnd)]
    fn get_append_window_end(this: &JsBridge, sb_id: &str) -> f64;

    #[wasm_bindgen(method, js_name = setAppendWindowEnd)]
    fn set_append_window_end(this: &JsBridge, sb_id: &str, end: f64);

    // ── Fetch API ──────────────────────────────────────────────────────────
    // fetch(url, {mode:'cors', headers:{'Range':…}})  →  Promise<FetchResult>
    // FetchResult = { data: Uint8Array, finalUrl: string, durationMs: number }
    #[wasm_bindgen(method, js_name = fetchBytes)]
    fn fetch_bytes(this: &JsBridge, url: &str, range_header: Option<String>) -> Promise;

    // fetch(url, {method:'HEAD'})  →  Promise<bool>
    #[wasm_bindgen(method, js_name = checkExists)]
    fn check_exists(this: &JsBridge, url: &str) -> Promise;
}

// ── FetchResult extern type ───────────────────────────────────────────────────

/// Typed accessors for the plain JS object `{ data, finalUrl, durationMs }`
/// returned by `bridge.fetchBytes()`.
#[wasm_bindgen]
extern "C" {
    type FetchResult;

    #[wasm_bindgen(method, getter)]
    fn data(this: &FetchResult) -> Uint8Array;

    #[wasm_bindgen(method, getter, js_name = finalUrl)]
    fn final_url(this: &FetchResult) -> String;

    #[wasm_bindgen(method, getter, js_name = durationMs)]
    fn duration_ms(this: &FetchResult) -> f64;
}

// ── BridgeClock ───────────────────────────────────────────────────────────────

/// [`Clock`] implementation that calls `performance.now()` and `Date.now()`
/// via the JS bridge.
///
/// Stores `start_ms` (captured at construction) so that [`Clock::now_ms`]
/// returns elapsed time rather than an absolute `performance.now()` value.
struct BridgeClock {
    bridge:   JsBridge,
    start_ms: f64,
}

impl BridgeClock {
    fn new(bridge: JsBridge) -> Arc<Self> {
        let start_ms = bridge.now_ms();
        Arc::new(Self { bridge, start_ms })
    }
}

// Safety: WASM is single-threaded.  `JsBridge` is a JS reference-counted
// handle that is never accessed from multiple threads simultaneously.
unsafe impl Send for BridgeClock {}
unsafe impl Sync for BridgeClock {}

impl Clock for BridgeClock {
    /// Monotonic milliseconds elapsed since this clock was created.
    fn now_ms(&self) -> f64 {
        self.bridge.now_ms() - self.start_ms
    }

    /// Seconds since the Unix epoch (`Date.now() / 1000`).
    fn unix_s(&self) -> f64 {
        self.bridge.unix_now_s()
    }
}

// ── BridgeSourceBuffer ────────────────────────────────────────────────────────

/// [`SourceBuffer`] implementation that delegates every operation to the JS
/// bridge.
///
/// `sb_id` is the opaque identifier returned by `bridge.addSourceBuffer()`.
/// The JS side uses it to look up the corresponding browser `SourceBuffer`
/// object.
///
/// Append and remove operations return Promises in JS (resolved on the
/// `updateend` event), so they are event-driven rather than polled — an
/// improvement over the polling approach used by the previous `web_sys`-based
/// implementation.
struct BridgeSourceBuffer {
    bridge:    JsBridge,
    sb_id:     String,
    mime_type: String,
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

// ── BridgeMediaSource ─────────────────────────────────────────────────────────

/// [`MediaSource`] implementation that delegates every operation to the JS
/// bridge.
///
/// `mime_to_id` maps a MIME type string (e.g. `"video/mp4;codecs=avc1…"`) to
/// the opaque source-buffer ID assigned by `bridge.addSourceBuffer()`, so that
/// `remove_source_buffer` can find the right browser object by MIME type.
struct BridgeMediaSource {
    bridge:     JsBridge,
    mime_to_id: HashMap<String, String>,
}

impl BridgeMediaSource {
    fn new(bridge: JsBridge) -> Self {
        Self { bridge, mime_to_id: HashMap::new() }
    }
}

#[async_trait(?Send)]
impl MediaSource for BridgeMediaSource {
    fn ready_state(&self) -> ReadyState {
        match self.bridge.get_ready_state().as_str() {
            "open"  => ReadyState::Open,
            "ended" => ReadyState::Ended,
            _       => ReadyState::Closed,
        }
    }

    fn duration(&self) -> Option<f64> {
        let d = self.bridge.get_duration();
        if d.is_nan() || d < 0.0 { None } else { Some(d) }
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
        let sb_id = self.bridge
            .add_source_buffer(mime_type)
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        self.mime_to_id.insert(mime_type.to_owned(), sb_id.clone());
        Ok(Box::new(BridgeSourceBuffer {
            bridge:    self.bridge.clone().unchecked_into(),
            sb_id,
            mime_type: mime_type.to_owned(),
        }))
    }

    fn remove_source_buffer(&mut self, mime_type: &str) -> Result<(), DashError> {
        let sb_id = self.mime_to_id.remove(mime_type).ok_or_else(|| {
            DashError::SourceBufferNotFound { mime_type: mime_type.to_owned() }
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

// ── BridgeFetcher ─────────────────────────────────────────────────────────────

/// [`SegmentFetcher`] implementation backed by `window.fetch()` called from
/// the JS bridge.
///
/// Byte-range requests (for `SegmentBase` and `SegmentList` with
/// `@mediaRange`) pass a `Range: bytes=<start>-<end>` header to JS, where the
/// explicit `fetch()` call is visible.
struct BridgeFetcher {
    bridge: JsBridge,
}

#[async_trait(?Send)]
impl SegmentFetcher for BridgeFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let range_header = request
            .byte_range
            .map(|(s, e)| format!("bytes={s}-{e}"));

        let resolved = JsFuture::from(
            self.bridge.fetch_bytes(request.url.as_str(), range_header),
        )
        .await
        .map_err(|e| FetchError::Network {
            url:     request.url.clone(),
            message: js_err(e),
        })?;

        // Cast the resolved JS object to our typed FetchResult accessor.
        let result: FetchResult = resolved.unchecked_into();
        let data              = Bytes::from(result.data().to_vec());
        let bytes_received    = data.len() as u64;
        let transfer_duration_ms = result.duration_ms() as u64;
        let final_url         = Url::parse(&result.final_url())
            .unwrap_or_else(|_| request.url.clone());

        Ok(FetchResponse { final_url, data, transfer_duration_ms, bytes_received })
    }

    async fn check_existence(&self, url: &Url) -> Result<bool, FetchError> {
        let ok = JsFuture::from(self.bridge.check_exists(url.as_str()))
            .await
            .map_err(|e| FetchError::Network { url: url.clone(), message: js_err(e) })?;
        Ok(ok.as_bool().unwrap_or(false))
    }

    fn abort(&self) {
        // No-op: dropping the in-flight JsFuture cancels the underlying fetch
        // in most browsers when no AbortController is attached.
    }
}

// ── WASM entry point ──────────────────────────────────────────────────────────

/// Initialise WASM infrastructure (panic hook + tracing).
///
/// Called automatically by the WASM module initialiser before any exported
/// function runs.
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();
}

/// Start MPEG-DASH playback.
///
/// # Arguments
///
/// * `mpd_url` — fully qualified URL to the `.mpd` manifest.
/// * `bridge`  — JS object that implements every browser Web API that dash-rs
///   needs.  See `index.html` for the full annotated implementation.
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
        .map_err(|e| JsValue::from_str(&js_err(e)))?;

    // ── Build the DashSession ──────────────────────────────────────────────

    let url = Url::parse(&mpd_url)
        .map_err(|e| JsValue::from_str(&format!("invalid MPD URL: {e}")))?;

    // Each struct gets its own clone of the bridge (cheap JS reference copy).
    let clock   = BridgeClock::new(bridge.clone().unchecked_into());
    let fetcher = BridgeFetcher { bridge: bridge.clone().unchecked_into() };
    let ms      = BridgeMediaSource::new(bridge.clone().unchecked_into());

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
            .map_err(|e| JsValue::from_str(&js_err(e)))?;
        session.apply_live_pts_remap();
        let seek_time = session.live_presentation_start_s();
        bridge.seek_to(seek_time);
        tracing::info!(seek_time, "live stream: PTS remapped; seeked video element to live edge");
    }

    session.notify_playing();

    // ── Segment fetch loop ─────────────────────────────────────────────────

    let mut video_done = !session.has_video();
    let mut audio_done = !session.has_audio();

    // Use whichever track is present for the buffer gate.  If only audio
    // was set up (e.g. video skipped because the browser doesn't support
    // HEVC), gating on the absent video track would always return 0 ms and
    // we'd spin-fetch audio with no back-pressure.
    let gate_type = if session.has_video() {
        MediaType::Video
    } else {
        MediaType::Audio
    };

    loop {
        if video_done && audio_done {
            break;
        }

        // Buffer gate: mirrors dash.js ScheduleController.
        // JS: new Promise(r => setTimeout(r, 500))
        while session.estimated_buffer_level_ms(&gate_type) >= session.buffer_target_ms() {
            JsFuture::from(bridge.sleep(500.0)).await.ok();
        }

        if !video_done {
            let position_s = bridge.get_current_time();
            let metrics = session.current_metrics(MediaType::Video, position_s);
            match session.fetch_next_segment(MediaType::Video, &metrics).await {
                Ok(d) => tracing::debug!(
                    quality_index = d.index,
                    reason        = ?d.reason,
                    "video segment fetched"
                ),
                Err(DashError::EndOfStream { .. }) => {
                    tracing::info!("video: end of stream");
                    video_done = true;
                }
                Err(e) => return Err(JsValue::from_str(&e.to_string())),
            }
        }

        if !audio_done {
            let position_s = bridge.get_current_time();
            let metrics = session.current_metrics(MediaType::Audio, position_s);
            match session.fetch_next_segment(MediaType::Audio, &metrics).await {
                Ok(d) => tracing::debug!(
                    quality_index = d.index,
                    reason        = ?d.reason,
                    "audio segment fetched"
                ),
                Err(DashError::EndOfStream { .. }) => {
                    tracing::info!("audio: end of stream");
                    audio_done = true;
                }
                Err(e) => return Err(JsValue::from_str(&e.to_string())),
            }
        }
    }

    // ── Signal end of stream ───────────────────────────────────────────────
    // JS: mediaSource.endOfStream()
    session
        .end_of_stream()
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    tracing::info!("playback complete — end of stream signalled");
    Ok(())
}
