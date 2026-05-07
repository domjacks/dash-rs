//! HTTP fetch example.
//!
//! Demonstrates a real [`SegmentFetcher`] implementation backed by `reqwest`.
//! Fetches a publicly available MPEG-DASH manifest over HTTP, inspects the
//! available representations, fetches initialisation segments for both tracks,
//! then runs a short adaptive-bitrate segment loop with live throughput
//! measurements feeding the ABR controller.
//!
//! Run with:
//!   cargo run --example http_fetch
//!
//! Override the manifest URL with a first positional argument:
//!   cargo run --example http_fetch -- https://your-stream/manifest.mpd

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use async_trait::async_trait;
use bytes::Bytes;
use dash_rs::{
    fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher},
    media_source::stub::StubMediaSource,
    session::{DashSession, MediaType},
    DashError,
};
use url::Url;

// ── HttpSegmentFetcher ────────────────────────────────────────────────────────

/// A [`SegmentFetcher`] backed by a `reqwest` HTTP client.
///
/// After every successful fetch the measured throughput (bits per second) is
/// published into `last_throughput_bps` via an `AtomicU64` bit-cast so the
/// outer fetch loop can read real values back for the next ABR decision.
struct HttpSegmentFetcher {
    client: reqwest::Client,
    /// `f64::to_bits(throughput_bps)` after each completed fetch.
    last_throughput_bps: Arc<AtomicU64>,
}

impl HttpSegmentFetcher {
    fn new(throughput_tracker: Arc<AtomicU64>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("dash-rs-example/0.1")
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
        // Capture the final URL before consuming the response body.
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

        // Publish throughput so the outer loop can build accurate AbrMetrics.
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

    fn abort(&self) {
        // Dropping the in-flight future is sufficient to cancel a reqwest
        // request; there is no global cancellation handle needed here.
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

/// BBC R&D Big Buck Bunny MPEG-DASH test stream (common-init variant).
/// Uses `SegmentTemplate` with `$Number$` addressing — compatible with the
/// current `resolve_segment_url` implementation.
const DEFAULT_MPD_URL: &str = "https://rdmedia.bbc.co.uk/bbb/2/client_manifest-common_init.mpd";

/// Number of media segments to fetch per track after the init segment.
const SEGMENTS_TO_FETCH: u32 = 4;

#[tokio::main]
async fn main() -> Result<(), DashError> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // Accept an optional manifest URL as a positional CLI argument.
    let url_str = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_MPD_URL.to_owned());

    let manifest_url = Url::parse(&url_str).unwrap_or_else(|e| {
        eprintln!("error: invalid URL '{}': {}", url_str, e);
        std::process::exit(1);
    });

    println!("Manifest: {manifest_url}");
    println!();

    // 1. Shared throughput tracker — the fetcher writes it; the loop reads it.
    let throughput_tracker = Arc::new(AtomicU64::new(0u64));

    // 2. Open a stub media source.  No real decoder is attached; bytes are
    //    appended through the full MSE abstraction and then discarded.
    let mut media_source = StubMediaSource::new();
    media_source.open();

    // 3. Build the session.  This issues a real HTTP GET for the MPD.
    let fetcher = HttpSegmentFetcher::new(Arc::clone(&throughput_tracker));
    let mut session = DashSession::from_url(&manifest_url, fetcher, media_source).await?;

    // 4. Print available representations.
    println!("=== Video representations ===");
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

    // 5. Fetch initialisation segments — required before media segments so
    //    that a real decoder can read codec parameters from the moov box.
    println!("=== Init segments ===");
    match session.fetch_init_segment(MediaType::Video).await {
        Ok(n) => println!("  video  init: {n} bytes"),
        Err(e) => println!("  video  init skipped: {e}"),
    }
    match session.fetch_init_segment(MediaType::Audio).await {
        Ok(n) => println!("  audio  init: {n} bytes"),
        Err(e) => println!("  audio  init skipped: {e}"),
    }
    println!();

    // 6. Segment fetch loop.  Real throughput from the previous segment is
    //    fed into AbrMetrics so quality decisions react to actual bandwidth.
    println!("=== Fetching {SEGMENTS_TO_FETCH} segments ===");
    println!(
        "  {:<4}  {:<8}  {:<12}  {:<8}  {:<12}  {}",
        "seg", "vid-idx", "vid-reason", "aud-idx", "aud-reason", "video fetch"
    );

    for seg in 1..=SEGMENTS_TO_FETCH {
        // Read throughput measured by the fetcher during the previous segment.
        let throughput_bps = f64::from_bits(throughput_tracker.load(Ordering::Relaxed));
        // Use current_metrics() so SwitchHistoryRule sees the real current_index.
        let mut metrics = session.current_metrics(MediaType::Video);
        metrics.throughput_bps = throughput_bps;
        // Simple buffer model: assume each segment is 4 seconds of content.
        metrics.buffer_level_ms = (seg as f64) * 4_000.0;

        let t_video = Instant::now();
        let video_decision = session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await?;
        let video_ms = t_video.elapsed().as_millis();

        let audio_decision = session
            .fetch_next_segment(MediaType::Audio, &{
                let mut m = session.current_metrics(MediaType::Audio);
                m.throughput_bps = throughput_bps;
                m.buffer_level_ms = (seg as f64) * 4_000.0;
                m
            })
            .await?;

        // Re-read throughput after both fetches for the next iteration.
        let measured_bps = f64::from_bits(throughput_tracker.load(Ordering::Relaxed));

        println!(
            "  {:<4}  {:<8}  {:<12}  {:<8}  {:<12}  {:>4} ms  ({:.0} kbps measured)",
            seg,
            video_decision.index,
            format!("{:?}", video_decision.reason),
            audio_decision.index,
            format!("{:?}", audio_decision.reason),
            video_ms,
            measured_bps / 1_000.0,
        );
    }

    // 7. Signal end of stream.
    session.end_of_stream().await?;
    println!("\nEnd of stream. Done.");

    Ok(())
}
