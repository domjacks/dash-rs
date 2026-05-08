//! GStreamer-backed [`MediaSource`] and [`SourceBuffer`] implementations.
//!
//! This module lives alongside the `gstreamer_player` example rather than in
//! the library itself — concrete sink implementations belong with the example
//! that uses them, not in `src/`.  Only the traits (`MediaSource`,
//! `SourceBuffer`) live in the library.
//!
//! Enabled by the `sink-gstreamer` Cargo feature.  Requires GStreamer ≥ 1.14
//! headers at build time.  On macOS install via Homebrew:
//!
//! ```text
//! brew install gstreamer
//! ```
//!
//! # Architecture
//!
//! Each call to [`GstMediaSource::add_source_buffer`] creates an
//! `appsrc ! queue ! decodebin` chain inside the shared [`gst::Pipeline`].
//! When `decodebin` identifies the codec on the first fragment it fires
//! `pad-added`; the closure wires the new pad to the appropriate sink:
//!
//! ```text
//! appsrc ─► queue ─► decodebin ──(video pad)──► videoconvert ─► autovideosink
//!                               ╰─(audio pad)──► audioconvert ─► audioresample ─► autoaudiosink
//! ```
//!
//! Bytes reach `appsrc` through [`GstSourceBuffer::append_buffer`], which is
//! called by `dash_rs::media_source::sink::SourceBufferSink` inside
//! `dash_rs::session::DashSession`.
//!
//! # Main-thread requirement on macOS
//!
//! `autovideosink` on macOS selects `osxvideosink` or `glimagesink`.  Both
//! sinks require a running `NSApplication` before they can create a Cocoa
//! `NSWindow`.  In GStreamer ≥ 1.22 the sinks marshal their own Cocoa and
//! GL-context creation onto the main thread internally via
//! `g_main_context_invoke` / `perform_selector_on_main_thread`, so
//! `pad-added` can safely call [`wire_sink_chain`] on the GStreamer streaming
//! thread without any additional dispatch.
//!
//! The caller is responsible for ensuring `NSApplication` is initialised
//! before the pipeline transitions to Playing.  See `main.rs` for the
//! recommended setup: wrap `main()` with `gst::macos_main()`, run the
//! GStreamer bus loop on the main thread via `glib::MainLoop::run()`, and
//! push segments from a background OS thread.

use async_trait::async_trait;
// Conventional short aliases used throughout the gstreamer-rs ecosystem.
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use dash_rs::{
    error::DashError,
    media_source::{Chunk, MediaSource, ReadyState, SourceBuffer, TimeRange},
};

// Re-export so `main.rs` can use `gst_media_source::GstPipeline` if needed.
#[allow(unused_imports)]
pub use gst::Pipeline as GstPipeline;

// ── GstSourceBuffer ───────────────────────────────────────────────────────────

/// A [`SourceBuffer`] backed by a GStreamer [`gst_app::AppSrc`] element.
///
/// Created exclusively by [`GstMediaSource::add_source_buffer`]; do not
/// construct directly.
pub struct GstSourceBuffer {
    mime_type: String,
    appsrc: gst_app::AppSrc,
    is_updating: bool,
    timestamp_offset: f64,
    append_window_start: f64,
    append_window_end: f64,
    /// Accumulated buffered time ranges, built from chunks that carry timing.
    buffered: Vec<TimeRange>,
}

impl GstSourceBuffer {
    fn new(mime_type: impl Into<String>, appsrc: gst_app::AppSrc) -> Self {
        Self {
            mime_type: mime_type.into(),
            appsrc,
            is_updating: false,
            timestamp_offset: 0.0,
            append_window_start: 0.0,
            append_window_end: f64::INFINITY,
            buffered: Vec::new(),
        }
    }
}

#[async_trait(?Send)]
impl SourceBuffer for GstSourceBuffer {
    fn mime_type(&self) -> &str {
        &self.mime_type
    }

    fn is_updating(&self) -> bool {
        self.is_updating
    }

    fn buffered(&self) -> Vec<TimeRange> {
        self.buffered.clone()
    }

    fn timestamp_offset(&self) -> f64 {
        self.timestamp_offset
    }

    fn set_timestamp_offset(&mut self, offset: f64) {
        self.timestamp_offset = offset;
    }

    fn append_window_start(&self) -> f64 {
        self.append_window_start
    }

    fn set_append_window_start(&mut self, start: f64) {
        self.append_window_start = start;
    }

    fn append_window_end(&self) -> f64 {
        self.append_window_end
    }

    fn set_append_window_end(&mut self, end: f64) {
        self.append_window_end = end;
    }

    async fn append_buffer(&mut self, chunk: Chunk) -> Result<(), DashError> {
        if self.is_updating {
            return Err(DashError::SourceBufferUpdating);
        }

        self.is_updating = true;

        // Build a GStreamer buffer from the chunk bytes.
        let mut gst_buf = gst::Buffer::from_slice(chunk.data);

        // Stamp PTS and duration when the chunk carries timing info.
        if let Some(start) = chunk.start {
            let buf_mut = gst_buf
                .get_mut()
                .expect("freshly created buffer has no other owners");
            buf_mut.set_pts(gst::ClockTime::from_nseconds(
                (start * 1_000_000_000.0) as u64,
            ));
            if let Some(end) = chunk.end {
                let dur_ns = ((end - start) * 1_000_000_000.0) as u64;
                buf_mut.set_duration(gst::ClockTime::from_nseconds(dur_ns));
                // Track this range for buffered() queries.
                self.buffered.push(TimeRange::new(start, end));
            }
        }

        tracing::debug!(
            mime = %self.mime_type,
            label = %chunk.label,
            pts = ?gst_buf.pts(),
            bytes = gst_buf.size(),
            "pushing buffer to appsrc"
        );

        // push_buffer() is synchronous and normally non-blocking (default
        // max-bytes 200 KB / 100 ms).  For typical DASH segments (100 KB –
        // 2 MB) this returns immediately; backpressure only kicks in when
        // the pipeline stalls.
        self.appsrc
            .push_buffer(gst_buf)
            .map_err(|flow| DashError::AppendFailed(format!("appsrc flow error: {flow:?}")))?;

        self.is_updating = false;
        Ok(())
    }

    async fn remove(&mut self, start: f64, end: f64) -> Result<(), DashError> {
        tracing::warn!(
            mime = %self.mime_type,
            start,
            end,
            "GstSourceBuffer::remove is a no-op; GStreamer manages its own buffer"
        );
        self.buffered.retain(|r| r.end <= start || r.start >= end);
        Ok(())
    }

    async fn abort(&mut self) -> Result<(), DashError> {
        self.is_updating = false;
        // Flush the appsrc so GStreamer discards any queued data.
        self.appsrc.send_event(gst::event::FlushStart::new());
        self.appsrc.send_event(gst::event::FlushStop::new(true));
        tracing::debug!(mime = %self.mime_type, "GstSourceBuffer aborted");
        Ok(())
    }

    async fn change_type(&mut self, mime_type: &str) -> Result<(), DashError> {
        tracing::warn!(
            old = %self.mime_type,
            new = mime_type,
            "GstSourceBuffer::change_type: updating stored mime type only; \
             GStreamer caps renegotiation is not supported at runtime"
        );
        self.mime_type = mime_type.to_owned();
        Ok(())
    }
}

// ── GstMediaSource ────────────────────────────────────────────────────────────

/// A [`MediaSource`] backed by a GStreamer pipeline.
///
/// ## Lifecycle
///
/// ```text
/// let ms = GstMediaSource::new()?;       // pipeline created, ready_state = Open
/// let pipeline = ms.pipeline();          // clone the handle *before* moving ms
///
/// let session = DashSession::from_url(&url, fetcher, ms).await?;
/// // DashSession internally calls add_source_buffer() for each track, adding
/// // appsrc + decodebin chains to the pipeline.
///
/// session.media_source_mut().set_playing()?;  // transition to Playing
/// ```
pub struct GstMediaSource {
    pipeline: gst::Pipeline,
    ready_state: ReadyState,
    duration: Option<f64>,
    /// (mime_type, appsrc) for every source buffer that has been added.
    source_buffers: Vec<(String, gst_app::AppSrc)>,
    /// When `true`, a PTS/DTS + SEGMENT remapping probe is installed on each
    /// converter's sink pad by the `pad-added` callback.  The probe intercepts
    /// the qtdemux `NEWSEGMENT` event (replacing it with a zeroed TIME segment)
    /// and every decoded buffer (subtracting the first absolute PTS seen from
    /// all subsequent PTS/DTS values).  This remaps absolute Unix-epoch
    /// timestamps carried by CMAF live segments (e.g. BBC Two HD:
    /// PTS ≈ 1.778 × 10¹⁸ ns) to relative values starting from zero, allowing
    /// GStreamer's normal `sync=true` frame-pacing to work correctly.
    ///
    /// Set via [`Self::set_live_mode`] after the session is constructed but
    /// before [`Self::set_playing`] is called.  The `pad-added` callback reads
    /// the flag lazily when it fires during preroll, so the ordering is correct
    /// as long as `set_live_mode` is called before the pipeline transitions to
    /// Playing.
    live_mode: Arc<AtomicBool>,
}

impl GstMediaSource {
    /// Create a new empty GStreamer pipeline.
    ///
    /// Calls [`gst::init`] if GStreamer has not been initialised yet.
    pub fn new() -> Result<Self, DashError> {
        gst::init().map_err(|e| DashError::AppendFailed(format!("gst::init failed: {e}")))?;

        // On macOS, VideoToolbox (`vtdec` / `vtdec_hw`) requires `codec_data`
        // to be present in the H.264 caps *before* its first frame.  For
        // `avc3` streams (in-band SPS/PPS, no `avcC` box in the init
        // segment) `h264parse` synthesises `codec_data` from the first IDR
        // frame, but by that point vtdec has already tried — and failed — to
        // create its VideoToolbox session, posting a pipeline error.
        //
        // Downranking vtdec to NONE causes `decodebin` to prefer
        // `avdec_h264` (libav software decoder), which handles in-band
        // SPS/PPS natively and works reliably with DASH avc3 content.
        #[cfg(target_os = "macos")]
        for name in ["vtdec", "vtdec_hw"] {
            if let Some(factory) = gst::ElementFactory::find(name) {
                factory.set_rank(gst::Rank::NONE);
                tracing::debug!("lowered {name} rank to NONE (avc3 workaround)");
            }
        }

        let pipeline = gst::Pipeline::default();
        tracing::debug!("GstMediaSource: pipeline created");

        Ok(Self {
            pipeline,
            ready_state: ReadyState::Open,
            duration: None,
            source_buffers: Vec::new(),
            live_mode: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Return a cloned reference to the underlying GStreamer pipeline.
    ///
    /// Clone this **before** moving `GstMediaSource` into a `DashSession` so
    /// that the caller retains a handle for the bus loop and state transitions.
    pub fn pipeline(&self) -> gst::Pipeline {
        self.pipeline.clone()
    }

    /// Transition the pipeline to `Playing` state.
    ///
    /// Call this after all source buffers have been created (i.e. after
    /// `DashSession::from_url` has returned) and before the first
    /// `DashSession::fetch_next_segment` call.
    pub fn set_playing(&self) -> Result<(), DashError> {
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| DashError::AppendFailed(format!("set_state(Playing) failed: {e}")))?;
        tracing::info!("GstMediaSource: pipeline set to Playing");
        Ok(())
    }

    /// Enable live-stream mode.
    ///
    /// When set to `true`, a buffer pad probe is installed on each converter's
    /// sink pad by the `pad-added` callback.  The probe remaps the absolute
    /// Unix-epoch PTS/DTS carried by CMAF live segments (e.g. BBC Two HD:
    /// PTS ≈ 1.778 × 10¹⁸ ns) to values starting from zero, so that
    /// `autovideosink` renders frames immediately at the correct frame rate
    /// instead of scheduling them ~56 years in the future.
    ///
    /// The sink's `sync` property is left at its default (`true`), which keeps
    /// GStreamer's frame-pacing mechanism intact and avoids the burst-then-gap
    /// stutter that `sync=false` would cause.
    ///
    /// Call this **after** [`DashSession::from_url`] (which calls
    /// `add_source_buffer`) but **before** [`Self::set_playing`].  The
    /// `pad-added` callback reads the flag lazily when it fires during preroll,
    /// so the ordering is correct as long as `set_live_mode` is called before
    /// the pipeline transitions to Playing.
    pub fn set_live_mode(&mut self, live: bool) {
        self.live_mode.store(live, Ordering::Relaxed);
        tracing::info!(live, "GstMediaSource: live_mode updated");
    }
}

#[async_trait(?Send)]
impl MediaSource for GstMediaSource {
    fn ready_state(&self) -> ReadyState {
        self.ready_state.clone()
    }

    fn duration(&self) -> Option<f64> {
        self.duration
    }

    fn set_duration(&mut self, duration: f64) -> Result<(), DashError> {
        self.duration = Some(duration);
        Ok(())
    }

    fn add_source_buffer(&mut self, mime_type: &str) -> Result<Box<dyn SourceBuffer>, DashError> {
        if self.ready_state != ReadyState::Open {
            return Err(DashError::MediaSourceNotOpen {
                state: self.ready_state.clone(),
            });
        }

        tracing::info!(mime = mime_type, "GstMediaSource: adding source buffer");

        // ── Build the caps for the appsrc ────────────────────────────────────
        // fMP4 (fragmented ISOBMFF) segments — the QuickTime container covers
        // both video/mp4 and audio/mp4 DASH tracks.
        let caps = gst::Caps::builder("video/quicktime")
            .field("variant", "iso-fragmented")
            .build();

        // ── Create appsrc ────────────────────────────────────────────────────
        let appsrc = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Bytes)
            .stream_type(gst_app::AppStreamType::Stream)
            // VOD content: is_live(false) enables proper preroll so both
            // audio and video sinks start clock-synchronised.  is_live(true)
            // skips preroll, which causes the pipeline clock to start before
            // the video decoder initialises, dropping early frames.
            .is_live(false)
            // fMP4 carries its own timestamps; do not generate synthetic ones.
            .do_timestamp(false)
            .build();

        // Disable the default 200 KB appsrc buffer limit so that push_buffer()
        // never returns a flow error when the pipeline is ahead by multiple
        // seconds of content.  The fetch-loop's buffer-level gate (12 s
        // target) bounds memory usage instead.
        appsrc.set_property("max-bytes", 0u64);

        // ── Create queue + decodebin ─────────────────────────────────────────
        let is_video = mime_type.starts_with("video/");
        let track_tag = if is_video { "video" } else { "audio" };

        let queue = gst::ElementFactory::make("queue")
            .name(format!("queue-{track_tag}"))
            .build()
            .map_err(|e| DashError::AppendFailed(format!("failed to create queue: {e}")))?;

        let decodebin = gst::ElementFactory::make("decodebin")
            .name(format!("decodebin-{track_tag}"))
            .build()
            .map_err(|e| DashError::AppendFailed(format!("failed to create decodebin: {e}")))?;

        // ── Add to pipeline and link the static part ─────────────────────────
        self.pipeline
            .add_many([appsrc.upcast_ref::<gst::Element>(), &queue, &decodebin])
            .map_err(|e| DashError::AppendFailed(format!("pipeline.add_many failed: {e}")))?;

        gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), &queue, &decodebin])
            .map_err(|e| DashError::AppendFailed(format!("link_many failed: {e}")))?;

        // ── Wire decodebin's dynamic pads to sinks ───────────────────────────
        //
        // decodebin fires `pad-added` when it has parsed enough data to know
        // the codec.  We then create converter + sink elements, add them to
        // the live pipeline, and sync their state with the parent.
        //
        // All captured values must be 'static + Send.
        let pipeline_weak = self.pipeline.downgrade();
        let live_mode_arc = Arc::clone(&self.live_mode);

        decodebin.connect_pad_added(move |_elem: &gst::Element, src_pad: &gst::Pad| {
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };

            // Read the live-mode flag at the time the pad fires (not at
            // closure-creation time) so that a `set_live_mode(true)` call
            // made after `from_url` but before `set_playing` is reflected.
            let remap_pts = live_mode_arc.load(Ordering::Relaxed);

            // Determine audio vs video from the newly negotiated caps.
            let caps = src_pad
                .current_caps()
                .unwrap_or_else(|| src_pad.query_caps(None));
            let structure = match caps.structure(0) {
                Some(s) => s,
                None => return,
            };
            let media_type = structure.name();

            tracing::info!(
                media_type = %media_type,
                "decodebin pad-added — wiring sink chain"
            );

            if media_type.starts_with("video/") {
                // Wire synchronously on the streaming thread.  GStreamer ≥ 1.22
                // osxvideosink / glimagesink internally marshal their Cocoa
                // NSWindow and GL-context creation onto the main thread via
                // g_main_context_invoke — we do not need to do it from here.
                // Deferring via MainContext::invoke would leave the pad unlinked
                // during decodebin's preroll window, causing the pipeline to
                // stall or error before video playback begins.
                wire_sink_chain(
                    &pipeline,
                    src_pad,
                    "videoconvert",
                    None,
                    "autovideosink",
                    remap_pts,
                );
            } else if media_type.starts_with("audio/") {
                wire_sink_chain(
                    &pipeline,
                    src_pad,
                    "audioconvert",
                    Some("audioresample"),
                    "autoaudiosink",
                    remap_pts,
                );
            } else {
                tracing::warn!(media_type = %media_type, "unrecognised pad type; skipping");
            }
        });

        // ── Store the appsrc handle and return the source buffer ─────────────
        self.source_buffers
            .push((mime_type.to_owned(), appsrc.clone()));

        Ok(Box::new(GstSourceBuffer::new(mime_type, appsrc)))
    }

    fn remove_source_buffer(&mut self, mime_type: &str) -> Result<(), DashError> {
        let pos = self
            .source_buffers
            .iter()
            .position(|(m, _)| m == mime_type)
            .ok_or_else(|| DashError::SourceBufferNotFound {
                mime_type: mime_type.to_owned(),
            })?;

        let (_, appsrc) = self.source_buffers.remove(pos);
        // Flush downstream before removal.
        let _ = appsrc.end_of_stream();
        if let Err(err) = self.pipeline.remove(appsrc.upcast_ref::<gst::Element>()) {
            tracing::warn!("failed to remove appsrc from pipeline: {err}");
        }
        Ok(())
    }

    async fn end_of_stream(&mut self) -> Result<(), DashError> {
        tracing::info!("GstMediaSource: signalling EOS on all appsrc elements");
        for (mime, appsrc) in &self.source_buffers {
            appsrc.end_of_stream().map_err(|e| {
                DashError::AppendFailed(format!("appsrc EOS ({mime}) failed: {e:?}"))
            })?;
        }
        self.ready_state = ReadyState::Ended;
        Ok(())
    }

    fn close(&mut self) {
        tracing::info!("GstMediaSource: closing pipeline");
        let _ = self.pipeline.set_state(gst::State::Null);
        self.source_buffers.clear();
        self.ready_state = ReadyState::Closed;
    }
}

impl Drop for GstMediaSource {
    fn drop(&mut self) {
        if self.ready_state != ReadyState::Closed {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Create `converter [? resample] sink` elements, add them to `pipeline`,
/// link `src_pad` into the head of the chain, and sync state with parent.
///
/// When `remap_pts` is `true` a buffer pad probe is installed on the
/// converter's sink pad.  The probe subtracts the first buffer's PTS from
/// every subsequent buffer's PTS, and independently does the same for DTS.
/// This remaps absolute Unix-epoch timestamps (e.g. BBC Two HD live CMAF
/// segments: PTS ≈ 1.778 × 10¹⁸ ns) to relative values starting at zero.
/// Combined with the default `sync=true` on the sink, GStreamer paces frames
/// at the correct rate from the moment the first frame arrives, eliminating
/// both the "frame scheduled 56 years in the future" stall and the burst-gap
/// stutter that `sync=false` causes.
fn wire_sink_chain(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    converter_name: &str,
    resample_name: Option<&str>,
    sink_name: &str,
    remap_pts: bool,
) {
    let make = |name: &str| -> Option<gst::Element> {
        match gst::ElementFactory::make(name).build() {
            Ok(e) => Some(e),
            Err(err) => {
                tracing::error!("failed to create {name}: {err}");
                None
            }
        }
    };

    let converter = match make(converter_name) {
        Some(e) => e,
        None => return,
    };
    let resample: Option<gst::Element> = resample_name.and_then(make);
    let sink = match make(sink_name) {
        Some(e) => e,
        None => return,
    };

    // Add all new elements to the live pipeline.
    let mut to_add: Vec<&gst::Element> = vec![&converter];
    if let Some(ref r) = resample {
        to_add.push(r);
    }
    to_add.push(&sink);

    if let Err(err) = pipeline.add_many(to_add.as_slice()) {
        tracing::error!("pipeline.add_many (sink chain) failed: {err}");
        return;
    }

    // Link the chain.
    let link_result: Result<(), _> = if let Some(ref r) = resample {
        gst::Element::link_many([&converter, r, &sink])
    } else {
        gst::Element::link_many([&converter, &sink])
    };
    if let Err(err) = link_result {
        tracing::error!("link_many (sink chain) failed: {err}");
        return;
    }

    // Connect decodebin's dynamic pad to the head of the chain.
    let converter_sink = match converter.static_pad("sink") {
        Some(p) => p,
        None => {
            tracing::error!("{converter_name} has no static sink pad");
            return;
        }
    };
    if let Err(err) = src_pad.link(&converter_sink) {
        tracing::error!("pad link failed: {err}");
        return;
    }

    // Install a combined SEGMENT + BUFFER probe on the converter's sink pad.
    //
    // CMAF live streams (e.g. BBC Two HD) carry absolute Unix-epoch PTS values
    // in their fMP4 `tfdt` boxes (≈ 1.778 × 10¹⁸ ns).  qtdemux emits a
    // NEWSEGMENT event whose `start` field equals that same absolute value.
    // GStreamer computes `running_time = pts − segment.start`, so:
    //
    //   • Buffer probe alone: pts → 0, but segment.start still absolute →
    //     running_time = 0 − 1.778×10¹⁸ → unsigned underflow → sink
    //     schedules every frame 56 years in the future → no video.
    //
    // Fix: intercept both the SEGMENT event and every BUFFER.
    //   1. Replace every SEGMENT event with a zeroed segment
    //      {start=0, time=0, base=0, rate=1.0}.
    //   2. On each buffer: subtract the first absolute PTS seen from pts/dts.
    //
    // Result: running_time = (pts_abs − offset) − 0 = small relative value;
    // the sink's sync=true paces frames at the correct rate from t=0.
    if remap_pts {
        let pts_off: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let dts_off: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

        converter_sink.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_pad, info| {
                // ── SEGMENT event ───────────────────────────────────────────
                // Check type without borrowing info.data mutably yet.
                let is_segment = if let Some(gst::PadProbeData::Event(ref ev)) = info.data {
                    ev.type_() == gst::EventType::Segment
                } else {
                    false
                };

                if is_segment {
                    // Replace with a fresh zeroed TIME segment so that
                    // running_time = remapped_pts − segment.start = remapped_pts.
                    let new_seg = gst::FormattedSegment::<gst::ClockTime>::new();
                    info.data = Some(gst::PadProbeData::Event(gst::event::Segment::new(&new_seg)));
                    tracing::info!("live PTS probe: replaced SEGMENT with zeroed TIME segment");
                    return gst::PadProbeReturn::Ok;
                }

                // ── BUFFER ──────────────────────────────────────────────────
                if let Some(gst::PadProbeData::Buffer(ref mut buf)) = info.data {
                    let buf_mut = buf.make_mut();

                    if let Some(pts) = buf_mut.pts() {
                        let off = *pts_off.lock().unwrap().get_or_insert(pts.nseconds());
                        let new_pts = pts.nseconds().saturating_sub(off);
                        tracing::debug!(
                            pts_abs = pts.nseconds(),
                            pts_new = new_pts,
                            "live PTS probe: remapped pts"
                        );
                        buf_mut.set_pts(gst::ClockTime::from_nseconds(new_pts));
                    }

                    if let Some(dts) = buf_mut.dts() {
                        let off = *dts_off.lock().unwrap().get_or_insert(dts.nseconds());
                        buf_mut.set_dts(gst::ClockTime::from_nseconds(
                            dts.nseconds().saturating_sub(off),
                        ));
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );
        tracing::info!(
            converter = converter_name,
            "live PTS/DTS remapping probe installed"
        );
    }

    // Sync each new element to the pipeline's running state.
    let all: &[&gst::Element] = if let Some(ref r) = resample {
        &[&converter, r, &sink]
    } else {
        &[&converter, &sink]
    };
    for el in all {
        if let Err(err) = el.sync_state_with_parent() {
            tracing::error!("sync_state_with_parent failed for {}: {err}", el.name());
        }
    }

    tracing::info!(
        converter = converter_name,
        resample = resample_name.unwrap_or("none"),
        sink = sink_name,
        "sink chain wired and synced"
    );
}
