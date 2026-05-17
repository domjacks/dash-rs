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
//!                   ├─ Player::from_url  (manifest fetch + parse)
//!                   ├─ fetch_init_segment x2  (video + audio)
//!                   ├─ set_playing()           (pipeline → Playing)
//!                   └─ segment fetch loop      (ABR + push bytes)
//! ```

mod driver;
mod fetcher;
mod gst_media_source;

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc,
};

use dash_rs::{DashError, Player};
use driver::GstPlaybackDriver;
use fetcher::HttpSegmentFetcher;
use gst::prelude::*;
use gst_media_source::GstMediaSource;
use gstreamer as gst;
use url::Url;

// ── Fetch / session task ──────────────────────────────────────────────────────

const DEFAULT_MPD_URL: &str = "https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd";

/// Runs inside the tokio runtime on the background OS thread.
async fn run_session(
    manifest_url: Url,
    media_source: GstMediaSource,
    throughput_tracker: Arc<AtomicU64>,
    is_paused: Arc<AtomicBool>,
    is_buffering: Arc<AtomicBool>,
) -> Result<(), DashError> {
    let fetcher = HttpSegmentFetcher::new(Arc::clone(&throughput_tracker));

    // Grab the pipeline handle before media_source is moved into Player::from_url.
    let pipeline = media_source.pipeline();
    let driver = GstPlaybackDriver::new(is_paused, is_buffering, throughput_tracker, pipeline);

    // Player::from_url fetches + parses the MPD, opens the media source, and
    // creates the source buffers.  GstMediaSource::open() is a no-op (the
    // pipeline is already Open at construction time).
    let mut player = Player::from_url(&manifest_url, fetcher, media_source, driver).await?;

    // Print available representations.
    if player.session().has_video() {
        println!("\n=== Video representations ===");
        for (i, rep) in player.session().video_representations().iter().enumerate() {
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
    if player.session().has_audio() {
        println!("=== Audio representations ===");
        for (i, rep) in player.session().audio_representations().iter().enumerate() {
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

    println!(
        "\n=== Fetching all segments (buffer target: {:.0} s) ===",
        player.session().buffer_target_ms() / 1_000.0
    );
    println!(
        "  {:<4}  {:<8}  {:<12}  {:<8}  {:<12}  {:<10}  {}",
        "seg", "vid-idx", "vid-reason", "aud-idx", "aud-reason", "buf-est", "video fetch"
    );

    player.run().await?;

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

    // Shared buffering-state flag: true while GStreamer reports < 100 % buffer.
    let is_buffering = Arc::new(AtomicBool::new(false));

    // Channel used to receive the fetch result from the background thread.
    let (done_tx, done_rx) = mpsc::channel::<Result<(), DashError>>();

    // Spawn a background OS thread that owns the tokio runtime and the
    // session.  This keeps the main thread free for GStreamer's bus loop
    // (required for macOS video-sink rendering).
    let throughput_tracker_bg = Arc::clone(&throughput_tracker);
    let is_paused_bg = Arc::clone(&is_paused);
    let is_buffering_bg = Arc::clone(&is_buffering);
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
            is_buffering_bg,
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
    let is_buffering_w = Arc::clone(&is_buffering);
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
                MessageView::Buffering(b) => {
                    let pct = b.percent();
                    let stalled = pct < 100;
                    is_buffering_w.store(stalled, Ordering::Relaxed);
                    if stalled {
                        tracing::debug!(percent = pct, "GStreamer buffering");
                    } else {
                        tracing::debug!("GStreamer buffering complete");
                    }
                }
                MessageView::StateChanged(sc) => {
                    // Only log top-level pipeline state changes.
                    if msg.src().map(|s| *s == pipeline_w).unwrap_or(false) {
                        let new_state = sc.current();
                        // Propagate pause/resume to the fetch loop.
                        is_paused_w.store(new_state == gst::State::Paused, Ordering::Relaxed);
                        // A state transition to Playing clears any buffering flag
                        // set while the pipeline was pre-rolling.
                        if new_state == gst::State::Playing {
                            is_buffering_w.store(false, Ordering::Relaxed);
                        }
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
