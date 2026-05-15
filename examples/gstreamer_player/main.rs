//! GStreamer playback example.
//!
//! Demonstrates a complete DASH playback stack:
//!
//! - Real HTTP manifest + segment fetching via `reqwest`
//! - Adaptive bitrate selection via [`dash_rs::abr::AbrController`]
//! - Media bytes fed into a GStreamer pipeline for hardware-accelerated
//!   decoding and rendering (video to a window, audio to speakers)
//!
//! # Prerequisites
//!
//! ```text
//! brew install gstreamer          # macOS — installs all required plugins
//! ```
//!
//! GStreamer ≥ 1.14 must be on `PKG_CONFIG_PATH`.  If the build fails to
//! locate headers, prepend the framework bin directory:
//!
//! ```text
//! export PATH="/Library/Frameworks/GStreamer.framework/Versions/1.0/bin:$PATH"
//! ```
//!
//! # Running
//!
//! ```text
//! cargo run --features sink-gstreamer --example gstreamer_player
//! cargo run --features sink-gstreamer --example gstreamer_player -- https://your-stream/manifest.mpd
//! ```
//!
//! # Thread model
//!
//! On macOS, `autovideosink` delegates to `osxvideosink` or `glimagesink`,
//! both of which require GLib's main loop (and therefore macOS's CFRunLoop)
//! to be running on the main thread to create their Cocoa `NSWindow`.
//!
//! To satisfy this constraint the example does **not** use `#[tokio::main]`,
//! and uses `glib::MainLoop::run()` instead of a `bus.timed_pop()` poll loop:
//!
//! ```text
//! main() thread  ← glib::MainLoop::run()  (drives GLib/CFRunLoop for video sink)
//!   │               └─ bus.add_watch()    (EOS / error handling)
//!   │               └─ glib::timeout_add  (fetch-error detection)
//!   │
//!   └─ OS thread ← tokio multi-thread runtime
//!                   ├─ DashSession::from_url  (manifest fetch + parse)
//!                   ├─ fetch_init_segment x2  (video + audio)
//!                   ├─ set_playing()           (pipeline → Playing)
//!                   └─ segment fetch loop      (ABR + push bytes)
//! ```

mod gst_media_source;

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::Instant,
};

use async_trait::async_trait;
use bytes::Bytes;
use dash_rs::{
    fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher},
    session::{
        driver::{IterationSummary, PlaybackDriver},
        DashSession, MediaType,
    },
    DashError,
};
use gst::prelude::*;
use gst_media_source::GstMediaSource;
use gstreamer as gst;
use url::Url;

// ── HttpSegmentFetcher ────────────────────────────────────────────────────────
// (same design as examples/http_fetch.rs — duplicated here until the
//  fetch-http feature promotes it into the library)

struct HttpSegmentFetcher {
    client: reqwest::Client,
    last_throughput_bps: Arc<AtomicU64>,
}

impl HttpSegmentFetcher {
    fn new(throughput_tracker: Arc<AtomicU64>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("dash-rs-gstreamer-example/0.1")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            last_throughput_bps: throughput_tracker,
        }
    }
}

#[async_trait(?Send)]
impl SegmentFetcher for HttpSegmentFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let mut builder = self.client.get(request.url.as_str());
        if let Some((start, end)) = request.byte_range {
            builder = builder.header("Range", format!("bytes={}-{}", start, end));
        }

        let t0 = Instant::now();
        let resp = builder.send().await.map_err(|e| FetchError::Network {
            url: request.url.clone(),
            message: e.to_string(),
        })?;

        let status = resp.status().as_u16();
        let final_url = Url::parse(resp.url().as_str()).unwrap_or_else(|_| request.url.clone());

        if !resp.status().is_success() {
            return Err(FetchError::Http {
                status,
                url: request.url,
            });
        }

        let body: Bytes = resp.bytes().await.map_err(|e| FetchError::Network {
            url: final_url.clone(),
            message: e.to_string(),
        })?;

        let transfer_duration_ms = t0.elapsed().as_millis() as u64;
        let bytes_received = body.len() as u64;

        if transfer_duration_ms > 0 {
            let bps = (bytes_received as f64 * 8.0) / (transfer_duration_ms as f64 / 1_000.0);
            self.last_throughput_bps
                .store(bps.to_bits(), Ordering::Relaxed);
        }

        Ok(FetchResponse {
            final_url,
            data: body,
            transfer_duration_ms,
            bytes_received,
        })
    }

    async fn check_existence(&self, url: &Url) -> Result<bool, FetchError> {
        let resp =
            self.client
                .head(url.as_str())
                .send()
                .await
                .map_err(|e| FetchError::Network {
                    url: url.clone(),
                    message: e.to_string(),
                })?;
        Ok(resp.status().is_success())
    }

    fn abort(&self) {}
}

// ── Fetch / session task ──────────────────────────────────────────────────────

/// BBC R&D Big Buck Bunny MPEG-DASH test stream (common-init variant).
const DEFAULT_MPD_URL: &str = "https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd";

/// Runs inside the tokio runtime on the background OS thread.
async fn run_session(
    manifest_url: Url,
    media_source: GstMediaSource,
    throughput_tracker: Arc<AtomicU64>,
    is_paused: Arc<AtomicBool>,
) -> Result<(), DashError> {
    let fetcher = HttpSegmentFetcher::new(Arc::clone(&throughput_tracker));

    // Build session — fetches + parses the MPD, creates source buffers.
    let mut session: DashSession<HttpSegmentFetcher, GstMediaSource> =
        DashSession::from_url(&manifest_url, fetcher, media_source).await?;

    // Print available representations.
    if session.has_video() {
        println!("\n=== Video representations ===");
        for (i, rep) in session.video_representations().iter().enumerate() {
            println!(
                "  [{i}] id={:<12} {:>6} kbps  {}x{}",
                rep.id,
                rep.bandwidth / 1_000,
                rep.width.unwrap_or(0),
                rep.height.unwrap_or(0),
            );
        }
    } else {
        println!("\n=== Video representations ===  (none — audio-only stream)");
    }
    if session.has_audio() {
        println!("=== Audio representations ===");
        for (i, rep) in session.audio_representations().iter().enumerate() {
            println!(
                "  [{i}] id={:<12} {:>6} kbps",
                rep.id,
                rep.bandwidth / 1_000,
            );
        }
    } else {
        println!("=== Audio representations ===  (none — video-only stream)");
    }
    println!();

    // Fetch init segments (codec parameters for the decoder).
    println!("=== Init segments ===");
    if session.has_video() {
        match session.fetch_init_segment(MediaType::Video).await {
            Ok(n) => println!("  video  init: {n} bytes"),
            Err(e) => println!("  video  init skipped: {e}"),
        }
    }
    if session.has_audio() {
        match session.fetch_init_segment(MediaType::Audio).await {
            Ok(n) => println!("  audio  init: {n} bytes"),
            Err(e) => println!("  audio  init skipped: {e}"),
        }
    }

    // For live CMAF streams (e.g. BBC Two HD), the fMP4 `tfdt` box carries
    // absolute Unix-epoch timestamps.  GStreamer's monotonic pipeline clock
    // starts from zero (boot time), so with `sync=true` the sinks would
    // schedule frames ~56 years in the future and the pipeline would stall.
    // Setting live_mode=true disables sink sync so frames render immediately.
    if session.is_live() {
        tracing::info!("live stream — enabling live_mode (sink sync disabled)");
        session.media_source_mut().set_live_mode(true);
    }

    // Now that source buffers have received the init segments, transition the
    // pipeline to Playing so decoding and rendering begin.
    session.media_source_mut().set_playing()?;

    // ── GstPlaybackDriver ────────────────────────────────────────────────────

    /// [`PlaybackDriver`] for the GStreamer player.
    ///
    /// - Uses the session's own estimated position (no external renderer query).
    /// - Slaves the audio gate to the video buffer level + 1 s so that audio
    ///   and video stay in lock-step (mirrors dash.js ScheduleController).
    /// - Enables buffer pruning so old data is evicted from GStreamer's appsrc.
    /// - Pushes a console progress row after every segment pair.
    struct GstPlaybackDriver {
        is_paused: Arc<AtomicBool>,
        throughput_tracker: Arc<AtomicU64>,
        /// Session's estimated playback position — updated at iteration start.
        cached_pos_s: f64,
        seg: u32,
    }

    impl GstPlaybackDriver {
        fn new(is_paused: Arc<AtomicBool>, throughput_tracker: Arc<AtomicU64>) -> Self {
            Self {
                is_paused,
                throughput_tracker,
                cached_pos_s: 0.0,
                seg: 0,
            }
        }
    }

    #[async_trait(?Send)]
    impl PlaybackDriver for GstPlaybackDriver {
        fn position_s(&self) -> f64 {
            self.cached_pos_s
        }

        fn is_paused(&self) -> bool {
            self.is_paused.load(Ordering::Relaxed)
        }

        fn throughput_override_bps(&self) -> Option<f64> {
            Some(f64::from_bits(
                self.throughput_tracker.load(Ordering::Relaxed),
            ))
        }

        fn audio_gate_target_ms(&self, video_buffer_ms: f64, default_ms: f64) -> f64 {
            // Slave audio target to video level so audio never races ahead.
            // For audio-only streams video_buffer_ms is 0, so we fall back
            // to the session default.
            if video_buffer_ms > 0.0 {
                video_buffer_ms.min(default_ms) + 1_000.0
            } else {
                default_ms
            }
        }

        fn enable_pruning(&self) -> bool {
            true
        }

        fn on_iteration_start(&mut self, session_position_s: f64) {
            self.cached_pos_s = session_position_s;
            self.seg += 1;
        }

        fn on_iteration(&mut self, s: &IterationSummary<'_>) {
            let measured_bps = f64::from_bits(self.throughput_tracker.load(Ordering::Relaxed));
            let buf_ms = if s.video_buffer_ms > 0.0 {
                s.video_buffer_ms
            } else {
                s.audio_buffer_ms
            };

            let (vid_idx, vid_reason, video_ms) = s
                .video_decision
                .map(|d| {
                    (
                        d.index.to_string(),
                        format!("{:?}", d.reason),
                        s.last_transfer_ms as u128,
                    )
                })
                .unwrap_or_else(|| ("done".into(), "-".into(), 0));
            let (aud_idx, aud_reason) = s
                .audio_decision
                .map(|d| (d.index.to_string(), format!("{:?}", d.reason)))
                .unwrap_or_else(|| ("done".into(), "-".into()));

            println!(
                "  {:<4}  {:<8}  {:<12}  {:<8}  {:<12}  {:>7.1} s   {:>4} ms  ({:.0} kbps)",
                self.seg,
                vid_idx,
                vid_reason,
                aud_idx,
                aud_reason,
                buf_ms / 1_000.0,
                video_ms,
                measured_bps / 1_000.0,
            );
        }
    }

    println!(
        "\n=== Fetching all segments (buffer target: {:.0} s) ===",
        session.buffer_target_ms() / 1_000.0
    );
    println!(
        "  {:<4}  {:<8}  {:<12}  {:<8}  {:<12}  {:<10}  {}",
        "seg", "vid-idx", "vid-reason", "aud-idx", "aud-reason", "buf-est", "video fetch"
    );

    let mut driver = GstPlaybackDriver::new(is_paused, throughput_tracker);
    session.run(&mut driver).await?;

    println!("\nAll segments fetched — waiting for pipeline EOS…");

    Ok(())
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    // On macOS, GStreamer video sinks require NSApplication to be initialised
    // on the main thread before any window or GL context is created.
    // gst::macos_main() calls gst_macos_main(), which invokes
    // [NSApplication sharedApplication] and runs our closure on the Cocoa
    // main thread — eliminating the "An NSApplication needs to be running on
    // the main thread" warning and enabling video output.
    //
    // On other platforms the closure is invoked directly.
    gst::macos_main(run);
}

fn run() {
    // Initialise tracing before GStreamer so library log events are captured.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // Initialise GStreamer (also called inside GstMediaSource::new, but
    // calling it here first gives cleaner error messages).
    if let Err(e) = gst::init() {
        eprintln!("fatal: GStreamer init failed: {e}");
        eprintln!("hint: ensure GStreamer ≥ 1.14 is installed (brew install gstreamer)");
        std::process::exit(1);
    }

    // Parse optional manifest URL argument.
    let url_str = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_MPD_URL.to_owned());
    let manifest_url = Url::parse(&url_str).unwrap_or_else(|e| {
        eprintln!("error: invalid URL '{}': {}", url_str, e);
        std::process::exit(1);
    });

    println!("dash-rs GStreamer player");
    println!("Manifest: {manifest_url}");

    // Create the GstMediaSource and grab a pipeline handle *before* moving
    // media_source into the session (which happens on the background thread).
    let media_source = GstMediaSource::new().unwrap_or_else(|e| {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    });
    let pipeline = media_source.pipeline();

    // Shared throughput tracker.
    let throughput_tracker = Arc::new(AtomicU64::new(0u64));

    // Shared pause-state flag updated from the GLib bus watch on the main
    // thread and read by the background fetch loop.
    let is_paused = Arc::new(AtomicBool::new(false));

    // Channel used to receive the fetch result from the background thread.
    let (done_tx, done_rx) = mpsc::channel::<Result<(), DashError>>();

    // Spawn a background OS thread that owns the tokio runtime and the
    // session.  This keeps the main thread free for GStreamer's bus loop
    // (required for macOS video-sink rendering).
    let throughput_tracker_bg = Arc::clone(&throughput_tracker);
    let is_paused_bg = Arc::clone(&is_paused);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");

        let result = rt.block_on(run_session(
            manifest_url,
            media_source,
            throughput_tracker_bg,
            is_paused_bg,
        ));

        done_tx.send(result).ok();
    });

    // ── GStreamer bus loop (runs on main thread) ───────────────────────────
    //
    // We use glib::MainLoop + bus.add_watch() instead of bus.timed_pop() so
    // that the GLib main loop runs on the main thread.  On macOS, GLib's
    // main loop integrates with CFRunLoop, which is required for
    // autovideosink (osxvideosink / glimagesink) to create its Cocoa
    // NSWindow.  bus.timed_pop() only blocks — it never spins CFRunLoop,
    // so the video sink window is never realised.

    let bus = pipeline.bus().expect("pipeline has no bus");
    let main_loop = gst::glib::MainLoop::new(None, false);

    // Bus watch — callback fires on the GLib main context (main thread).
    let main_loop_w = main_loop.clone();
    let pipeline_w = pipeline.clone();
    let is_paused_w = Arc::clone(&is_paused);
    let _watch = bus
        .add_watch(move |_bus, msg| {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => {
                    println!("GStreamer EOS — playback finished.");
                    main_loop_w.quit();
                }
                MessageView::Error(err) => {
                    // Always print to stderr so the cause is visible even
                    // when the tracing subscriber is filtered.
                    eprintln!(
                        "GStreamer ERROR\n  element : {}\n  message : {}\n  debug   : {}",
                        msg.src()
                            .map(|s: &gst::Object| s.path_string().to_string())
                            .unwrap_or_else(|| "<unknown>".into()),
                        err.error(),
                        err.debug().unwrap_or_else(|| "<no debug>".into()),
                    );
                    main_loop_w.quit();
                }
                MessageView::Warning(warn) => {
                    eprintln!(
                        "GStreamer WARNING\n  element : {}\n  message : {}\n  debug   : {}",
                        msg.src()
                            .map(|s: &gst::Object| s.path_string().to_string())
                            .unwrap_or_else(|| "<unknown>".into()),
                        warn.error(),
                        warn.debug().unwrap_or_else(|| "<no debug>".into()),
                    );
                }
                MessageView::StateChanged(sc) => {
                    // Only log top-level pipeline state changes.
                    if msg.src().map(|s| *s == pipeline_w).unwrap_or(false) {
                        let new_state = sc.current();
                        // Propagate pause/resume to the fetch loop.
                        is_paused_w.store(new_state == gst::State::Paused, Ordering::Relaxed);
                        tracing::info!(
                            old = ?sc.old(),
                            new = ?new_state,
                            "pipeline state changed"
                        );
                    }
                }
                _ => {}
            }
            gst::glib::ControlFlow::Continue
        })
        .expect("Failed to add bus watch");

    // Periodic check for fetch errors that arrive before GStreamer EOS.
    // The timer fires on the same GLib main context as the bus watch.
    let main_loop_t = main_loop.clone();
    let _timer =
        gst::glib::timeout_add(std::time::Duration::from_millis(200), move || match done_rx
            .try_recv()
        {
            Ok(Err(e)) => {
                eprintln!("fetch error: {e}");
                main_loop_t.quit();
                gst::glib::ControlFlow::Break
            }
            _ => gst::glib::ControlFlow::Continue,
        });

    // Blocks here, running the GLib / CFRunLoop event loop on the main
    // thread.  autovideosink can now create its Cocoa window.
    main_loop.run();

    // Tear down the pipeline before exiting.
    let _ = pipeline.set_state(gst::State::Null);
    println!("Done.");
}
