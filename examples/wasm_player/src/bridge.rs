//! [`JsBridge`] extern type — the single JS object that implements every
//! browser Web API that dash-rs needs.
//!
//! All methods are `pub(crate)` so that the implementation modules
//! (`clock`, `source_buffer`, `media_source`, `fetcher`, `driver`) can call
//! them without re-exporting the full extern block.
//!
//! The JavaScript implementation of each method is the educational reference
//! — open `player.js` to read the exact Web API calls.

use js_sys::{Float64Array, Promise, Uint8Array};
use wasm_bindgen::prelude::*;

// ── JsBridge ──────────────────────────────────────────────────────────────────

/// The JavaScript bridge object passed from `app.js` into [`super::play`].
///
/// Methods that can throw synchronously use `#[wasm_bindgen(catch)]` so that
/// JavaScript exceptions surface as `Result::Err` in Rust rather than panics.
/// Methods that return a `Promise` surface errors via the rejected branch of
/// [`wasm_bindgen_futures::JsFuture`].
#[wasm_bindgen]
extern "C" {
    pub type JsBridge;

    // ── Clock ─────────────────────────────────────────────────────────────
    // performance.now() — monotonic milliseconds since navigation start.
    #[wasm_bindgen(method, js_name = nowMs)]
    pub(crate) fn now_ms(this: &JsBridge) -> f64;

    // Date.now() / 1000 — seconds since Unix epoch.
    #[wasm_bindgen(method, js_name = unixNowS)]
    pub(crate) fn unix_now_s(this: &JsBridge) -> f64;

    // ── Utility ───────────────────────────────────────────────────────────
    // new Promise(r => setTimeout(r, ms))
    #[wasm_bindgen(method)]
    pub(crate) fn sleep(this: &JsBridge, ms: f64) -> Promise;

    // video.currentTime = time_s
    #[wasm_bindgen(method, js_name = seekTo)]
    pub(crate) fn seek_to(this: &JsBridge, time_s: f64);

    // video.currentTime
    #[wasm_bindgen(method, js_name = getCurrentTime)]
    pub(crate) fn get_current_time(this: &JsBridge) -> f64;

    // video.paused
    #[wasm_bindgen(method, js_name = isPaused)]
    pub(crate) fn is_paused(this: &JsBridge) -> bool;

    // !video.paused && video.readyState < HTMLMediaElement.HAVE_FUTURE_DATA
    #[wasm_bindgen(method, js_name = isBuffering)]
    pub(crate) fn is_buffering(this: &JsBridge) -> bool;

    // ── MediaSource lifecycle ─────────────────────────────────────────────
    // new MediaSource() + URL.createObjectURL() + video.src= + sourceopen
    #[wasm_bindgen(method, js_name = setupMediaSource)]
    pub(crate) fn setup_media_source(this: &JsBridge) -> Promise;

    // MediaSource.isTypeSupported(mimeType)
    #[wasm_bindgen(method, js_name = isTypeSupported)]
    pub(crate) fn is_type_supported(this: &JsBridge, mime_type: &str) -> bool;

    // mediaSource.readyState  →  "open" | "ended" | "closed"
    #[wasm_bindgen(method, js_name = getReadyState)]
    pub(crate) fn get_ready_state(this: &JsBridge) -> String;

    // mediaSource.duration
    #[wasm_bindgen(method, js_name = getDuration)]
    pub(crate) fn get_duration(this: &JsBridge) -> f64;

    // mediaSource.duration = d
    #[wasm_bindgen(method, catch, js_name = setDuration)]
    pub(crate) fn set_duration(this: &JsBridge, d: f64) -> Result<(), JsValue>;

    // mediaSource.addSourceBuffer(mimeType)  →  opaque buffer id
    #[wasm_bindgen(method, catch, js_name = addSourceBuffer)]
    pub(crate) fn add_source_buffer(this: &JsBridge, mime_type: &str) -> Result<String, JsValue>;

    // mediaSource.removeSourceBuffer(sourceBuffers[id])
    #[wasm_bindgen(method, catch, js_name = removeSourceBuffer)]
    pub(crate) fn remove_source_buffer_by_id(this: &JsBridge, sb_id: &str) -> Result<(), JsValue>;

    // mediaSource.endOfStream()
    #[wasm_bindgen(method, catch, js_name = endOfStream)]
    pub(crate) fn end_of_stream(this: &JsBridge) -> Result<(), JsValue>;

    // URL.revokeObjectURL(objectUrl)
    #[wasm_bindgen(method, js_name = close)]
    pub(crate) fn close_bridge(this: &JsBridge);

    // ── SourceBuffer operations ────────────────────────────────────────────
    // sb.appendBuffer(data) + updateend event  →  Promise<void>
    #[wasm_bindgen(method, js_name = appendBuffer)]
    pub(crate) fn append_buffer(this: &JsBridge, sb_id: &str, data: &Uint8Array) -> Promise;

    // sb.remove(start, end) + updateend event  →  Promise<void>
    #[wasm_bindgen(method, js_name = removeRange)]
    pub(crate) fn remove_range(this: &JsBridge, sb_id: &str, start: f64, end: f64) -> Promise;

    // sb.abort()
    #[wasm_bindgen(method, catch, js_name = abortBuffer)]
    pub(crate) fn abort_buffer(this: &JsBridge, sb_id: &str) -> Result<(), JsValue>;

    // sb.changeType(mimeType)
    #[wasm_bindgen(method, catch, js_name = changeType)]
    pub(crate) fn change_type(this: &JsBridge, sb_id: &str, mime_type: &str)
        -> Result<(), JsValue>;

    // sb.buffered  →  flat Float64Array [start0, end0, start1, end1, …]
    #[wasm_bindgen(method, js_name = getBuffered)]
    pub(crate) fn get_buffered(this: &JsBridge, sb_id: &str) -> Float64Array;

    // sb.updating
    #[wasm_bindgen(method, js_name = isUpdating)]
    pub(crate) fn is_updating(this: &JsBridge, sb_id: &str) -> bool;

    // sb.timestampOffset
    #[wasm_bindgen(method, js_name = getTimestampOffset)]
    pub(crate) fn get_timestamp_offset(this: &JsBridge, sb_id: &str) -> f64;

    #[wasm_bindgen(method, js_name = setTimestampOffset)]
    pub(crate) fn set_timestamp_offset(this: &JsBridge, sb_id: &str, offset: f64);

    // sb.appendWindowStart
    #[wasm_bindgen(method, js_name = getAppendWindowStart)]
    pub(crate) fn get_append_window_start(this: &JsBridge, sb_id: &str) -> f64;

    #[wasm_bindgen(method, js_name = setAppendWindowStart)]
    pub(crate) fn set_append_window_start(this: &JsBridge, sb_id: &str, start: f64);

    // sb.appendWindowEnd
    #[wasm_bindgen(method, js_name = getAppendWindowEnd)]
    pub(crate) fn get_append_window_end(this: &JsBridge, sb_id: &str) -> f64;

    #[wasm_bindgen(method, js_name = setAppendWindowEnd)]
    pub(crate) fn set_append_window_end(this: &JsBridge, sb_id: &str, end: f64);

    // ── Fetch API ──────────────────────────────────────────────────────────
    // fetch(url, {mode:'cors', headers:{'Range':…}})  →  Promise<FetchResult>
    // FetchResult = { data: Uint8Array, finalUrl: string, durationMs: number }
    #[wasm_bindgen(method, js_name = fetchBytes)]
    pub(crate) fn fetch_bytes(this: &JsBridge, url: &str, range_header: Option<String>) -> Promise;

    // fetch(url, {method:'HEAD'})  →  Promise<bool>
    #[wasm_bindgen(method, js_name = checkExists)]
    pub(crate) fn check_exists(this: &JsBridge, url: &str) -> Promise;

    // Debug panel status push — compact JSON string; JS calls JSON.parse on it.
    #[wasm_bindgen(method, js_name = reportStatus)]
    pub(crate) fn report_status(this: &JsBridge, json: &str);
}

// ── js_err helper ─────────────────────────────────────────────────────────────

/// Format a `JsValue` (which may be a `DOMException`, `Error`, or plain value)
/// into a human-readable string.
pub(crate) fn js_err(val: JsValue) -> String {
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
        (Some(n), _) if !n.is_empty() => n,
        (_, Some(m)) if !m.is_empty() => m,
        _ => js_sys::JSON::stringify(&val)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| "(opaque JS error)".to_owned()),
    }
}
