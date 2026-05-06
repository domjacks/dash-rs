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
//! On macOS, `autovideosink` delegates to `osxvideosink`, which **must** run
//! from the Cocoa main thread.  To satisfy this constraint the example does
//! **not** use `#[tokio::main]`:
//!
//! ```text
//! main() thread  ← GStreamer bus loop (Cocoa / video-sink safe)
//!   │
//!   └─ OS thread ← tokio multi-thread runtime
//!                   ├─ DashSession::from_url  (manifest fetch + parse)
//!                   ├─ fetch_init_segment x2  (video + audio)
//!                   ├─ set_playing()           (pipeline → Playing)
//!                   └─ segment fetch loop      (ABR + push bytes)
//! ```

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::Instant,
};

use async_trait::async_trait;
use bytes::Bytes;
use dash_rs::{
    abr::AbrMetrics,
    fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher},
    media_source::gstreamer::GstMediaSource,
    session::{DashSession, MediaType},
    DashError,
};
use gstreamer as gst;
use gst::prelude::*;
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

#[async_trait]
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
        let final_url =
            Url::parse(resp.url().as_str()).unwrap_or_else(|_| request.url.clone());

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
        let resp = self
            .client
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
const DEFAULT_MPD_URL: &str =
    "https://rdmedia.bbc.co.uk/bbb/2/client_manifest-common_init.mpd";

const SEGMENTS_TO_FETCH: u32 = 8;

/// Runs inside the tokio runtime on the background OS thread.
async fn run_session(
    manifest_url: Url,
    media_source: GstMediaSource,
    throughput_tracker: Arc<AtomicU64>,
) -> Result<(), DashError> {
    let fetcher = HttpSegmentFetcher::new(Arc::clone(&throughput_tracker));

    // Build session — fetches + parses the MPD, creates source buffers.
    let mut session: DashSession<HttpSegmentFetcher, GstMediaSource> =
        DashSession::from_url(&manifest_url, fetcher, media_source).await?;

    // Print available representations.
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
    println!("=== Audio representations ===");
    for (i, rep) in session.audio_representations().iter().enumerate() {
        println!(
            "  [{i}] id={:<12} {:>6} kbps",
            rep.id,
            rep.bandwidth / 1_000,
        );
    }
    println!();

    // Fetch init segments (codec parameters for the decoder).
    println!("=== Init segments ===");
    match session.fetch_init_segment(MediaType::Video).await {
        Ok(n) => println!("  video  init: {n} bytes"),
        Err(e) => println!("  video  init skipped: {e}"),
    }
    match session.fetch_init_segment(MediaType::Audio).await {
        Ok(n) => println!("  audio  init: {n} bytes"),
        Err(e) => println!("  audio  init skipped: {e}"),
    }

    // Now that source buffers have received the init segments, transition the
    // pipeline to Playing so decoding and rendering begin.
    session.media_source_mut().set_playing()?;

    // Segment fetch loop.
    println!("\n=== Fetching {SEGMENTS_TO_FETCH} segments ===");
    println!(
        "  {:<4}  {:<8}  {:<12}  {:<8}  {:<12}  {}",
        "seg", "vid-idx", "vid-reason", "aud-idx", "aud-reason", "video fetch"
    );

    for seg in 1..=SEGMENTS_TO_FETCH {
        let throughput_bps =
            f64::from_bits(throughput_tracker.load(Ordering::Relaxed));
        let buffer_level_ms = (seg as f64) * 4_000.0;
        let metrics = AbrMetrics::new(throughput_bps, buffer_level_ms);

        let t_video = Instant::now();
        let video_decision = session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await?;
        let video_ms = t_video.elapsed().as_millis();

        let audio_decision = session
            .fetch_next_segment(MediaType::Audio, &metrics)
            .await?;

        let measured_bps =
            f64::from_bits(throughput_tracker.load(Ordering::Relaxed));

        println!(
            "  {:<4}  {:<8}  {:<12}  {:<8}  {:<12}  {:>4} ms  ({:.0} kbps)",
            seg,
            video_decision.index,
            format!("{:?}", video_decision.reason),
            audio_decision.index,
            format!("{:?}", audio_decision.reason),
            video_ms,
            measured_bps / 1_000.0,
        );
    }

    // Signal the pipeline that no more data is coming.
    session.end_of_stream().await?;
    println!("\nAll segments fetched — waiting for pipeline EOS…");

    Ok(())
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    // Initialise tracing before GStreamer so library log events are captured.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // Initialise GStreamer (also called inside GstMediaSource::new, but
    // calling it here first gives cleaner error messages).
    if let Err(e) = gst::init() {
        eprintln!("fatal: GStreamer init failed: {e}");
        eprintln!(
            "hint: ensure GStreamer ≥ 1.14 is installed (brew install gstreamer)"
        );
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

    // Channel used to receive the fetch result from the background thread.
    let (done_tx, done_rx) = mpsc::channel::<Result<(), DashError>>();

    // Spawn a background OS thread that owns the tokio runtime and the
    // session.  This keeps the main thread free for GStreamer's bus loop
    // (required for macOS video-sink rendering).
    let throughput_tracker_bg = Arc::clone(&throughput_tracker);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");

        let result = rt.block_on(run_session(
            manifest_url,
            media_source,
            throughput_tracker_bg,
        ));

        done_tx.send(result).ok();
    });

    // ── GStreamer bus loop (runs on main thread) ───────────────────────────

    let bus = pipeline.bus().expect("pipeline has no bus");

    loop {
        // Poll with a short timeout so we can also check the done channel.
        match bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
            Some(msg) => {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(..) => {
                        println!("GStreamer EOS — playback finished.");
                        break;
                    }
                    MessageView::Error(err) => {
                        eprintln!(
                            "GStreamer error from {}: {} ({})",
                            msg.src()
                                .map(|s: &gst::Object| s.path_string().to_string())
                                .unwrap_or_else(|| "unknown".into()),
                            err.error(),
                            err.debug().unwrap_or_default(),
                        );
                        break;
                    }
                    MessageView::Warning(warn) => {
                        tracing::warn!(
                            src = ?msg.src().map(|s: &gst::Object| s.path_string()),
                            msg = %warn.error(),
                            "GStreamer warning"
                        );
                    }
                    MessageView::StateChanged(sc) => {
                        // Only log top-level pipeline state changes.
                        if msg.src().map(|s| *s == pipeline).unwrap_or(false) {
                            tracing::info!(
                                old = ?sc.old(),
                                new = ?sc.current(),
                                "pipeline state changed"
                            );
                        }
                    }
                    _ => {}
                }
            }
            None => {
                // Timeout — check whether the fetch task finished with an
                // error before GStreamer had a chance to send EOS.
                if let Ok(result) = done_rx.try_recv() {
                    if let Err(e) = result {
                        eprintln!("fetch error: {e}");
                        break;
                    }
                    // Fetch finished successfully; keep waiting for EOS.
                }
            }
        }
    }

    // Tear down the pipeline before exiting.
    let _ = pipeline.set_state(gst::State::Null);
    println!("Done.");
}
