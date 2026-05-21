//! GStreamer-backed [`MediaBuffer`] implementation.
//!
//! This module lives alongside the `gstreamer_player` example rather than in
//! the library itself — concrete sink implementations belong with the example
//! that uses them, not in `src/`.  Only the trait ([`MediaBuffer`]) lives in
//! the library.
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
//! [`GstMediaBuffer::add_track`] creates an `appsrc ! queue ! decodebin` chain
//! inside the shared [`gst::Pipeline`] for each track.  When `decodebin`
//! identifies the codec on the first fragment it fires `pad-added`; the closure
//! wires the new pad to the appropriate sink:
//!
//! ```text
//! appsrc ─► queue ─► decodebin ──(video pad)──► videoconvert ─► autovideosink
//!                               ╰─(audio pad)──► audioconvert ─► audioresample ─► autoaudiosink
//! ```
//!
//! Bytes reach `appsrc` through [`GstMediaBuffer::append`], which is called
//! by `DashSession` inside `run()`.
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
use bytes::Bytes;
// Conventional short aliases used throughout the gstreamer-rs ecosystem.
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use dash_rs::{
    error::DashError,
    media_buffer::{MediaBuffer, TimeRange},
    session::MediaType,
};

// Re-export so `main.rs` can use `gst_media_buffer::GstPipeline` if needed.
#[allow(unused_imports)]
pub use gst::Pipeline as GstPipeline;

// ── GstMediaBuffer ────────────────────────────────────────────────────────────

/// A [`MediaBuffer`] backed by a GStreamer pipeline.
///
/// ## Lifecycle
///
/// ```text
/// let buf = GstMediaBuffer::new()?;       // pipeline created
/// let pipeline = buf.pipeline();          // clone the handle *before* moving buf
///
/// let mut player = Player::from_url(&url, fetcher, buf, driver).await?;
/// // DashSession internally calls add_track() for each track, adding
/// // appsrc + decodebin chains to the pipeline.
///
/// // GstPlaybackDriver::on_session_started() transitions pipeline to Playing.
/// player.run().await?;
/// ```
pub struct GstMediaBuffer {
    pipeline: gst::Pipeline,
    /// `appsrc` for the video track, set by [`add_track`].
    video_appsrc: Option<gst_app::AppSrc>,
    /// `appsrc` for the audio track, set by [`add_track`].
    audio_appsrc: Option<gst_app::AppSrc>,
    /// When `true`, a PTS/DTS + SEGMENT remapping probe is installed on each
    /// converter's sink pad by the `pad-added` callback.  Set via
    /// [`set_live_mode`] before [`GstPlaybackDriver::on_session_started`]
    /// transitions the pipeline to Playing.
    live_mode: Arc<AtomicBool>,
    /// `true` once the pipeline has been set to Null (Drop guard).
    closed: bool,
}

impl GstMediaBuffer {
    /// Create a new empty GStreamer pipeline.
    ///
    /// Calls [`gst::init`] if GStreamer has not been initialised yet.
    pub fn new() -> Result<Self, DashError> {
        gst::init().map_err(|e| DashError::AppendFailed {
            message: format!("gst::init failed: {e}"),
        })?;

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
        tracing::debug!("GstMediaBuffer: pipeline created");

        Ok(Self {
            pipeline,
            video_appsrc: None,
            audio_appsrc: None,
            live_mode: Arc::new(AtomicBool::new(false)),
            closed: false,
        })
    }

    /// Return a cloned reference to the underlying GStreamer pipeline.
    ///
    /// Clone this **before** moving `GstMediaBuffer` into a `Player` so that
    /// the caller retains a handle for the bus loop and state transitions.
    pub fn pipeline(&self) -> gst::Pipeline {
        self.pipeline.clone()
    }

    /// Internal helper: build and wire an `appsrc ! queue ! decodebin` chain.
    fn add_appsrc(&mut self, mt: MediaType) -> Result<gst_app::AppSrc, DashError> {
        let track_tag = match mt {
            MediaType::Video => "video",
            MediaType::Audio => "audio",
        };

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
            // audio and video sinks start clock-synchronised.
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
        let queue = gst::ElementFactory::make("queue")
            .name(format!("queue-{track_tag}"))
            .build()
            .map_err(|e| DashError::AppendFailed {
                message: format!("failed to create queue: {e}"),
            })?;

        let decodebin = gst::ElementFactory::make("decodebin")
            .name(format!("decodebin-{track_tag}"))
            .build()
            .map_err(|e| DashError::AppendFailed {
                message: format!("failed to create decodebin: {e}"),
            })?;

        // ── Add to pipeline and link the static part ─────────────────────────
        self.pipeline
            .add_many([appsrc.upcast_ref::<gst::Element>(), &queue, &decodebin])
            .map_err(|e| DashError::AppendFailed {
                message: format!("pipeline.add_many failed: {e}"),
            })?;

        gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), &queue, &decodebin])
            .map_err(|e| DashError::AppendFailed {
                message: format!("link_many failed: {e}"),
            })?;

        // ── Wire decodebin's dynamic pads to sinks ───────────────────────────
        let pipeline_weak = self.pipeline.downgrade();
        let live_mode_arc = Arc::clone(&self.live_mode);

        decodebin.connect_pad_added(move |_elem: &gst::Element, src_pad: &gst::Pad| {
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };

            // Read the live-mode flag at the time the pad fires so that a
            // `set_live_mode(true)` call made after `from_url` but before
            // `set_playing` is reflected.
            let remap_pts = live_mode_arc.load(Ordering::Relaxed);

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

        Ok(appsrc)
    }
}

#[async_trait(?Send)]
impl MediaBuffer for GstMediaBuffer {
    fn set_duration(&mut self, _secs: f64) {
        // GStreamer determines duration from the stream itself; no explicit set needed.
    }

    /// Store the live-mode flag so the PTS-remapping probe fires when
    /// `pad-added` is called during preroll.  Must be called before the
    /// pipeline transitions to Playing (i.e. before
    /// `GstPlaybackDriver::on_session_started`).
    fn set_live_mode(&mut self, live: bool) {
        self.live_mode.store(live, Ordering::Relaxed);
        tracing::info!(live, "GstMediaBuffer: live_mode set");
    }

    async fn end_of_stream(&mut self) -> Result<(), DashError> {
        tracing::info!("GstMediaBuffer: signalling EOS on all appsrc elements");
        for appsrc in [&self.video_appsrc, &self.audio_appsrc]
            .iter()
            .filter_map(|x| x.as_ref())
        {
            appsrc
                .end_of_stream()
                .map_err(|e| DashError::AppendFailed {
                    message: format!("appsrc EOS failed: {e:?}"),
                })?;
        }
        Ok(())
    }

    /// Allocate an `appsrc ! queue ! decodebin` chain for `mt`.
    ///
    /// Returns `false` only if the GStreamer element factory calls fail,
    /// which would indicate a missing GStreamer plugin.
    fn add_track(&mut self, mt: MediaType, _mime_type: &str) -> bool {
        match self.add_appsrc(mt) {
            Ok(appsrc) => {
                match mt {
                    MediaType::Video => self.video_appsrc = Some(appsrc),
                    MediaType::Audio => self.audio_appsrc = Some(appsrc),
                }
                true
            }
            Err(e) => {
                tracing::error!("GstMediaBuffer: add_track({mt:?}) failed: {e}");
                false
            }
        }
    }

    async fn change_type(&mut self, mt: MediaType, mime_type: &str) -> Result<(), DashError> {
        tracing::warn!(
            ?mt,
            new = mime_type,
            "GstMediaBuffer::change_type: GStreamer caps renegotiation is \
             not supported at runtime; MIME stored for reference only"
        );
        Ok(())
    }

    fn set_timestamp_offset(&mut self, _mt: MediaType, _offset_s: f64) {
        // GStreamer handles decode-timestamp alignment via probe-based PTS
        // subtraction (set_live_mode); explicit offsetting is not needed.
    }

    fn set_append_window(&mut self, _mt: MediaType, _start: f64, _end: f64) {
        // GStreamer does not support MSE-style append windows; no-op.
    }

    async fn append(
        &mut self,
        mt: MediaType,
        data: Bytes,
        _is_init: bool,
    ) -> Result<(), DashError> {
        let appsrc = match mt {
            MediaType::Video => self.video_appsrc.as_ref(),
            MediaType::Audio => self.audio_appsrc.as_ref(),
        }
        .ok_or_else(|| DashError::AppendFailed {
            message: format!("{mt:?} track not initialised"),
        })?;

        let gst_buf = gst::Buffer::from_slice(data);

        tracing::debug!(
            ?mt,
            bytes = gst_buf.size(),
            "GstMediaBuffer: pushing buffer to appsrc"
        );

        appsrc
            .push_buffer(gst_buf)
            .map_err(|flow| DashError::AppendFailed {
                message: format!("appsrc flow error: {flow:?}"),
            })?;

        Ok(())
    }

    async fn remove_range(&mut self, mt: MediaType, start: f64, end: f64) -> Result<(), DashError> {
        tracing::warn!(
            ?mt,
            start,
            end,
            "GstMediaBuffer::remove_range is a no-op; GStreamer manages its own buffer"
        );
        Ok(())
    }

    /// Always returns `vec![]`.
    ///
    /// GStreamer does not expose buffered time ranges in the same way as MSE.
    /// [`has_buffer_tracking`][Self::has_buffer_tracking] returns `false` so
    /// that [`DashSession::current_metrics`] substitutes the session's own
    /// segment-count estimate, giving buffer-occupancy ABR rules a realistic
    /// level rather than a permanent zero.
    fn buffered(&self, _mt: MediaType) -> Vec<TimeRange> {
        vec![]
    }

    /// Returns `false` — GStreamer does not expose MSE-style buffered time
    /// ranges, so the session falls back to its own
    /// [`estimated_buffer_level_ms`][dash_rs::session::DashSession::estimated_buffer_level_ms]
    /// when computing ABR metrics.
    fn has_buffer_tracking(&self) -> bool {
        false
    }
}

impl Drop for GstMediaBuffer {
    fn drop(&mut self) {
        if !self.closed {
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
        use std::sync::Mutex;
        let pts_off: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let dts_off: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

        converter_sink.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_pad, info| {
                // ── SEGMENT event ───────────────────────────────────────────
                let is_segment = if let Some(gst::PadProbeData::Event(ref ev)) = info.data {
                    ev.type_() == gst::EventType::Segment
                } else {
                    false
                };

                if is_segment {
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
