//! WASM player example for dash-rs.
//!
//! Provides browser-native implementations of:
//!
//! | Trait | Type | Backing API |
//! |---|---|---|
//! | [`Clock`] | [`WasmClock`] | `performance.now()` / `Date.now()` |
//! | [`SegmentFetcher`] | [`WasmSegmentFetcher`] | `window.fetch()` |
//! | [`SourceBuffer`] | [`WasmSourceBuffer`] | `MSE SourceBuffer` |
//! | [`MediaSource`] | [`WasmMediaSource`] | `MSE MediaSource` |
//!
//! # Build
//!
//! ```sh
//! wasm-pack build examples/wasm_player --target web --out-dir pkg
//! ```
//!
//! Then serve the directory containing `index.html` with any static file server,
//! e.g. `python3 -m http.server 8080`, and open `http://localhost:8080`.
//!
//! Pass a custom manifest URL via the `?url=…` query parameter.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::Arc,
};

use async_trait::async_trait;
use bytes::Bytes;
use js_sys::Uint8Array;
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

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Yield to the JavaScript event loop for `ms` milliseconds.
///
/// Used to poll `SourceBuffer.updating` and `MediaSource.readyState` while
/// waiting for browser callbacks to fire.
async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        web_sys::window()
            .expect("no window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .expect("set_timeout failed");
    });
    let _ = JsFuture::from(promise).await;
}

/// Map a `JsValue` error to a `String` for reporting.
fn js_err(val: JsValue) -> String {
    val.as_string()
        .or_else(|| {
            js_sys::JSON::stringify(&val)
                .ok()
                .and_then(|s| s.as_string())
        })
        .unwrap_or_else(|| "(opaque JS error)".to_owned())
}

// ── WasmClock ─────────────────────────────────────────────────────────────────

/// [`Clock`] implementation backed by the browser's `performance.now()` and
/// `Date.now()`.
///
/// Only stores a plain `f64` so it is `Send + Sync` (required by the trait
/// bound) even though WASM has a single-threaded execution model.
pub struct WasmClock {
    /// `performance.now()` value recorded at construction time, used to
    /// convert absolute `performance.now()` readings into relative
    /// milliseconds.
    start_ms: f64,
}

impl WasmClock {
    /// Create a new clock anchored at the current instant.
    pub fn new() -> Self {
        let start_ms = web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(0.0);
        Self { start_ms }
    }

    /// Convenience constructor that wraps `self` in an `Arc`.
    pub fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

impl Default for WasmClock {
    fn default() -> Self {
        Self::new()
    }
}

// Safety: WASM is single-threaded; `WasmClock` only holds a plain `f64`.
unsafe impl Send for WasmClock {}
unsafe impl Sync for WasmClock {}

impl Clock for WasmClock {
    /// Monotonic elapsed time in milliseconds since this clock was created.
    fn now_ms(&self) -> f64 {
        let current = web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(0.0);
        current - self.start_ms
    }

    /// Current UTC time as seconds since the Unix epoch (`Date.now() / 1000`).
    fn unix_s(&self) -> f64 {
        js_sys::Date::now() / 1_000.0
    }
}

// ── WasmSegmentFetcher ────────────────────────────────────────────────────────

/// [`SegmentFetcher`] implementation backed by the browser's `window.fetch()`
/// API.
///
/// Supports optional byte-range requests via the `Range:` HTTP header.  All
/// fetches use CORS mode so cross-origin streams are supported when the server
/// sends appropriate `Access-Control-Allow-Origin` headers.
pub struct WasmSegmentFetcher;

#[async_trait(?Send)]
impl SegmentFetcher for WasmSegmentFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let opts = web_sys::RequestInit::new();
        opts.set_method("GET");
        opts.set_mode(web_sys::RequestMode::Cors);

        // Attach a Range header for byte-range sub-requests (SegmentBase /
        // SegmentList with @mediaRange).
        if let Some((start, end)) = request.byte_range {
            let headers = web_sys::Headers::new()
                .map_err(|_| FetchError::Network {
                    url: request.url.clone(),
                    message: "failed to create Headers".to_owned(),
                })?;
            headers
                .set("Range", &format!("bytes={}-{}", start, end))
                .map_err(|_| FetchError::Network {
                    url: request.url.clone(),
                    message: "failed to set Range header".to_owned(),
                })?;
            opts.set_headers_headers(&headers);
        }

        let js_request =
            web_sys::Request::new_with_str_and_init(request.url.as_str(), &opts).map_err(|_| {
                FetchError::InvalidUrl(request.url.to_string())
            })?;

        let window = web_sys::window().expect("no window");

        // Record wall-clock start for throughput measurement.
        let t0 = window
            .performance()
            .map(|p| p.now())
            .unwrap_or(0.0);

        let resp_value = JsFuture::from(window.fetch_with_request(&js_request))
            .await
            .map_err(|e| FetchError::Network {
                url: request.url.clone(),
                message: js_err(e),
            })?;

        let resp: web_sys::Response = resp_value.dyn_into().map_err(|_| FetchError::Network {
            url: request.url.clone(),
            message: "response is not a Response object".to_owned(),
        })?;

        let status = resp.status();
        if !resp.ok() {
            return Err(FetchError::Http {
                status,
                url: request.url,
            });
        }

        // Capture the final URL (after any redirects) before consuming the body.
        let final_url = Url::parse(resp.url().as_str()).unwrap_or_else(|_| request.url.clone());

        // Read the body as an ArrayBuffer.
        let body_promise = resp.array_buffer().map_err(|e| FetchError::Network {
            url: final_url.clone(),
            message: js_err(e),
        })?;
        let array_buf = JsFuture::from(body_promise)
            .await
            .map_err(|e| FetchError::Network {
                url: final_url.clone(),
                message: js_err(e),
            })?;

        let uint8_array = Uint8Array::new(&array_buf);
        let data = Bytes::from(uint8_array.to_vec());

        let t1 = web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(t0);
        let transfer_duration_ms = (t1 - t0).max(0.0) as u64;
        let bytes_received = data.len() as u64;

        Ok(FetchResponse {
            final_url,
            data,
            transfer_duration_ms,
            bytes_received,
        })
    }

    async fn check_existence(&self, url: &Url) -> Result<bool, FetchError> {
        let opts = web_sys::RequestInit::new();
        opts.set_method("HEAD");
        opts.set_mode(web_sys::RequestMode::Cors);

        let js_request = web_sys::Request::new_with_str_and_init(url.as_str(), &opts)
            .map_err(|_| FetchError::InvalidUrl(url.to_string()))?;

        let window = web_sys::window().expect("no window");
        let resp_value = JsFuture::from(window.fetch_with_request(&js_request))
            .await
            .map_err(|e| FetchError::Network {
                url: url.clone(),
                message: js_err(e),
            })?;

        let resp: web_sys::Response = resp_value.dyn_into().map_err(|_| FetchError::Network {
            url: url.clone(),
            message: "response is not a Response object".to_owned(),
        })?;

        Ok(resp.ok())
    }

    fn abort(&self) {
        // The Fetch API does not expose a global abort handle; dropping the
        // in-flight future is sufficient to cancel the request in most
        // browsers when an AbortController is not used.
    }
}

// ── WasmSourceBuffer ──────────────────────────────────────────────────────────

/// [`SourceBuffer`] implementation wrapping the browser's MSE `SourceBuffer`.
///
/// `append_buffer`, `remove`, and `abort` all poll `SourceBuffer.updating`
/// with a 4 ms `setTimeout` loop rather than registering persistent event
/// listeners.  This keeps the implementation simple and avoids closure
/// lifetime complexity at the cost of up to ~4 ms of extra latency per
/// operation — well within the tolerance for a segment-based player.
pub struct WasmSourceBuffer {
    mime_type: String,
    inner: web_sys::SourceBuffer,
}

impl WasmSourceBuffer {
    fn new(mime_type: String, inner: web_sys::SourceBuffer) -> Self {
        Self { mime_type, inner }
    }

    /// Convert the browser `TimeRanges` object to a `Vec<TimeRange>`.
    fn time_ranges_from_js(tr: web_sys::TimeRanges) -> Vec<TimeRange> {
        let len = tr.length();
        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            // `start()` / `end()` return `Result` in web-sys; ignore bad indices.
            if let (Ok(s), Ok(e)) = (tr.start(i), tr.end(i)) {
                out.push(TimeRange::new(s, e));
            }
        }
        out
    }

    /// Wait until `SourceBuffer.updating` is `false`.
    async fn wait_not_updating(inner: &web_sys::SourceBuffer) {
        while inner.updating() {
            sleep_ms(4).await;
        }
    }
}

#[async_trait(?Send)]
impl SourceBuffer for WasmSourceBuffer {
    fn mime_type(&self) -> &str {
        &self.mime_type
    }

    fn is_updating(&self) -> bool {
        self.inner.updating()
    }

    fn buffered(&self) -> Vec<TimeRange> {
        self.inner
            .buffered()
            .map(Self::time_ranges_from_js)
            .unwrap_or_default()
    }

    fn timestamp_offset(&self) -> f64 {
        self.inner.timestamp_offset()
    }

    fn set_timestamp_offset(&mut self, offset: f64) {
        // Ignore errors — only possible if the buffer is detached.
        let _ = self.inner.set_timestamp_offset(offset);
    }

    fn append_window_start(&self) -> f64 {
        self.inner.append_window_start()
    }

    fn set_append_window_start(&mut self, start: f64) {
        let _ = self.inner.set_append_window_start(start);
    }

    fn append_window_end(&self) -> f64 {
        self.inner.append_window_end()
    }

    fn set_append_window_end(&mut self, end: f64) {
        let _ = self.inner.set_append_window_end(end);
    }

    async fn append_buffer(&mut self, chunk: Chunk) -> Result<(), DashError> {
        // Serialise: wait for any prior append / remove to finish.
        Self::wait_not_updating(&self.inner).await;

        tracing::debug!(
            mime_type = %self.mime_type,
            label     = %chunk.label,
            bytes     = chunk.data.len(),
            "WasmSourceBuffer: appendBuffer"
        );

        let array = Uint8Array::from(chunk.data.as_ref());
        self.inner
            .append_buffer_with_array_buffer_view(&array)
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;

        // Wait for `updateend` by polling.
        Self::wait_not_updating(&self.inner).await;

        Ok(())
    }

    async fn remove(&mut self, start: f64, end: f64) -> Result<(), DashError> {
        Self::wait_not_updating(&self.inner).await;

        tracing::debug!(
            mime_type = %self.mime_type,
            start,
            end,
            "WasmSourceBuffer: remove"
        );

        self.inner
            .remove(start, end)
            .map_err(|e| DashError::RemoveFailed(js_err(e)))?;

        Self::wait_not_updating(&self.inner).await;

        Ok(())
    }

    async fn abort(&mut self) -> Result<(), DashError> {
        tracing::debug!(mime_type = %self.mime_type, "WasmSourceBuffer: abort");
        self.inner
            .abort()
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        Ok(())
    }

    async fn change_type(&mut self, mime_type: &str) -> Result<(), DashError> {
        tracing::debug!(
            old = %self.mime_type,
            new = mime_type,
            "WasmSourceBuffer: changeType"
        );
        self.inner
            .change_type(mime_type)
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        self.mime_type = mime_type.to_owned();
        Ok(())
    }
}

// ── WasmMediaSource ───────────────────────────────────────────────────────────

/// [`MediaSource`] implementation wrapping the browser's MSE `MediaSource`.
///
/// # Lifecycle
///
/// 1. Call [`WasmMediaSource::attach`] with a `<video>` element.  This
///    creates a `MediaSource` object, sets it as the video `src` via an object
///    URL, and awaits the `sourceopen` event before returning.
/// 2. Pass the `WasmMediaSource` to [`DashSession::from_url_with_clock`].
/// 3. The session calls [`add_source_buffer`] for each track and then feeds
///    segments until [`end_of_stream`] is called.
pub struct WasmMediaSource {
    inner: web_sys::MediaSource,
    /// Object URL created via `URL.createObjectURL(mediaSource)`.  Kept so
    /// we can call `URL.revokeObjectURL` on `close()`.
    blob_url: String,
    /// Tracks (mime_type, SourceBuffer) pairs so `remove_source_buffer` can
    /// find the right browser `SourceBuffer` without needing `SourceBufferList`.
    added_buffers: Vec<(String, web_sys::SourceBuffer)>,
}

impl WasmMediaSource {
    /// Create a `MediaSource`, attach it to `video_el`, and wait for
    /// `sourceopen` before returning.
    pub async fn attach(video_el: web_sys::HtmlVideoElement) -> Result<Self, JsValue> {
        let ms = web_sys::MediaSource::new()?;

        // Register a one-shot `sourceopen` listener using a oneshot channel
        // so we can `.await` it ergonomically.
        let (tx, rx) = futures::channel::oneshot::channel::<()>();
        let tx = Rc::new(RefCell::new(Some(tx)));
        let closure = Closure::once(move || {
            if let Some(sender) = tx.borrow_mut().take() {
                let _ = sender.send(());
            }
        });
        ms.set_onsourceopen(Some(closure.as_ref().unchecked_ref()));
        // `forget()` leaks ~pointer-sized memory once per session, which is
        // acceptable for a one-shot initialisation callback.
        closure.forget();

        // Attach to the video element — this triggers the browser to fire
        // `sourceopen` once the MediaSource is ready.
        let blob_url = web_sys::Url::create_object_url_with_source(&ms)?;
        video_el.set_src(&blob_url);

        // Await `sourceopen`.
        rx.await
            .map_err(|_| JsValue::from_str("sourceopen channel dropped unexpectedly"))?;

        tracing::info!("WasmMediaSource: sourceopen received — MediaSource is open");

        Ok(Self {
            inner: ms,
            blob_url,
            added_buffers: Vec::new(),
        })
    }
}

#[async_trait(?Send)]
impl MediaSource for WasmMediaSource {
    fn ready_state(&self) -> ReadyState {
        match self.inner.ready_state() {
            web_sys::MediaSourceReadyState::Open   => ReadyState::Open,
            web_sys::MediaSourceReadyState::Ended  => ReadyState::Ended,
            _                                       => ReadyState::Closed,
        }
    }

    fn duration(&self) -> Option<f64> {
        let d = self.inner.duration();
        if d.is_nan() || d < 0.0 {
            None
        } else {
            Some(d)
        }
    }

    fn set_duration(&mut self, duration: f64) -> Result<(), DashError> {
        // set_duration is a plain setter in web-sys (returns ()); JS exceptions
        // would surface as panics only in debug builds, which is acceptable here.
        self.inner.set_duration(duration);
        Ok(())
    }

    fn add_source_buffer(&mut self, mime_type: &str) -> Result<Box<dyn SourceBuffer>, DashError> {
        tracing::debug!(mime_type, "WasmMediaSource: addSourceBuffer");
        let sb = self
            .inner
            .add_source_buffer(mime_type)
            .map_err(|e| DashError::AppendFailed(js_err(e)))?;
        // Keep a handle so we can pass the reference to remove_source_buffer later.
        self.added_buffers.push((mime_type.to_owned(), sb.clone()));
        Ok(Box::new(WasmSourceBuffer::new(mime_type.to_owned(), sb)))
    }

    fn remove_source_buffer(&mut self, mime_type: &str) -> Result<(), DashError> {
        if let Some(pos) = self.added_buffers.iter().position(|(m, _)| m == mime_type) {
            let (_, sb) = self.added_buffers.remove(pos);
            self.inner
                .remove_source_buffer(&sb)
                .map_err(|e| DashError::AppendFailed(js_err(e)))?;
            tracing::debug!(mime_type, "WasmMediaSource: removeSourceBuffer");
            Ok(())
        } else {
            Err(DashError::SourceBufferNotFound {
                mime_type: mime_type.to_owned(),
            })
        }
    }

    async fn end_of_stream(&mut self) -> Result<(), DashError> {
        tracing::info!("WasmMediaSource: endOfStream");
        self.inner
            .end_of_stream()
            .map_err(|e| DashError::AppendFailed(js_err(e)))
    }

    fn close(&mut self) {
        tracing::info!("WasmMediaSource: close");
        // Revoke the object URL to free browser resources.
        let _ = web_sys::Url::revoke_object_url(&self.blob_url);
    }
}

// ── WASM entry point ──────────────────────────────────────────────────────────

/// Initialise WASM infrastructure.
///
/// Must be called once before any other exported function.  Sets up the
/// panic hook (so Rust panics appear as proper errors in the browser console)
/// and configures `tracing-wasm` as the global tracing subscriber.
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();
}

/// Start MPEG-DASH playback.
///
/// Looks for a `<video id="video">` element in the document, attaches a
/// `MediaSource`, and runs a continuous segment fetch loop until the stream
/// ends.
///
/// # Arguments
///
/// * `mpd_url` — fully qualified URL to the `.mpd` manifest.
///
/// # Usage from JavaScript
///
/// ```js
/// import init, { play } from './pkg/wasm_player.js';
/// await init();
/// await play('https://example.com/manifest.mpd');
/// ```
#[wasm_bindgen]
pub async fn play(mpd_url: String) -> Result<(), JsValue> {
    // ── Find the <video> element ───────────────────────────────────────────

    let window   = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let document = window.document().ok_or_else(|| JsValue::from_str("no document"))?;
    let video: web_sys::HtmlVideoElement = document
        .get_element_by_id("video")
        .ok_or_else(|| JsValue::from_str("no element with id=\"video\""))?
        .dyn_into()
        .map_err(|_| JsValue::from_str("#video is not a <video> element"))?;

    tracing::info!(mpd_url = %mpd_url, "dash-rs WASM player starting");

    // ── Create & attach MediaSource ────────────────────────────────────────

    let media_source = WasmMediaSource::attach(video)
        .await
        .map_err(|e| JsValue::from_str(&js_err(e)))?;

    // ── Build the DashSession ──────────────────────────────────────────────

    let url = Url::parse(&mpd_url)
        .map_err(|e| JsValue::from_str(&format!("invalid MPD URL: {e}")))?;

    let fetcher = WasmSegmentFetcher;
    let clock   = WasmClock::arc();

    let mut session =
        DashSession::from_url_with_clock(&url, fetcher, media_source, clock)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

    tracing::info!(
        has_video = session.has_video(),
        has_audio = session.has_audio(),
        is_live   = session.is_live(),
        "session constructed"
    );

    session.notify_playing();

    // ── Segment fetch loop ─────────────────────────────────────────────────
    //
    // Simple loop: fetch the next video segment, then the next audio segment.
    // The ABR controller uses initial placeholder metrics; a production player
    // would measure real throughput from WasmSegmentFetcher and feed it back.

    let mut video_done = !session.has_video();
    let mut audio_done = !session.has_audio();

    loop {
        if video_done && audio_done {
            break;
        }

        // ── Buffer gate (mirrors dash.js ScheduleController) ──────────────
        // Pause fetching while the buffer is ahead of the target level.
        while session.estimated_buffer_level_ms(&MediaType::Video)
            >= session.buffer_target_ms()
        {
            sleep_ms(500).await;
        }

        // ── Video segment ─────────────────────────────────────────────────
        if !video_done {
            let metrics = session.current_metrics(MediaType::Video);
            match session.fetch_next_segment(MediaType::Video, &metrics).await {
                Ok(decision) => {
                    tracing::debug!(
                        quality_index = decision.index,
                        reason        = ?decision.reason,
                        "video segment fetched"
                    );
                }
                Err(DashError::EndOfStream { .. }) => {
                    tracing::info!("video: end of stream");
                    video_done = true;
                }
                Err(e) => return Err(JsValue::from_str(&e.to_string())),
            }
        }

        // ── Audio segment ─────────────────────────────────────────────────
        if !audio_done {
            let metrics = session.current_metrics(MediaType::Audio);
            match session.fetch_next_segment(MediaType::Audio, &metrics).await {
                Ok(decision) => {
                    tracing::debug!(
                        quality_index = decision.index,
                        reason        = ?decision.reason,
                        "audio segment fetched"
                    );
                }
                Err(DashError::EndOfStream { .. }) => {
                    tracing::info!("audio: end of stream");
                    audio_done = true;
                }
                Err(e) => return Err(JsValue::from_str(&e.to_string())),
            }
        }
    }

    // ── Signal end of stream ───────────────────────────────────────────────

    session
        .end_of_stream()
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    tracing::info!("playback complete — end of stream signalled");

    Ok(())
}
