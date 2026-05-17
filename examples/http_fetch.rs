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
    session::{
        driver::{IterationSummary, PlaybackDriver},
        DashSession,
    },
    DashError, Player,
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
const DEFAULT_MPD_URL: &str = "https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd";

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
    //    `StubMediaSource` starts as Open by default — no explicit `open()`
    //    call is needed.
    let media_source = StubMediaSource::new();

    // 3. Build the session.  This issues a real HTTP GET for the MPD.
    let fetcher = HttpSegmentFetcher::new(Arc::clone(&throughput_tracker));
    let session = DashSession::from_url(&manifest_url, fetcher, media_source).await?;

    // 4. Print available representations.
    if session.has_video() {
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
    } else {
        println!("=== Video representations ===  (none — audio-only stream)");
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

    // 5. Segment fetch loop via PlaybackDriver.
    //    `run()` automatically fetches init segments before starting the loop,
    //    so no explicit `fetch_init_segment` call is needed here.
    //    Real throughput from the fetcher feeds the ABR controller each
    //    iteration; `should_continue` exits after SEGMENTS_TO_FETCH pairs.
    println!("=== Fetching {SEGMENTS_TO_FETCH} segments ===");
    println!(
        "  {:<4}  {:<8}  {:<12}  {:<8}  {:<12}  {}",
        "seg", "vid-idx", "vid-reason", "aud-idx", "aud-reason", "fetch ms"
    );

    /// [`PlaybackDriver`] for the HTTP fetch example.
    ///
    /// Stops the loop after [`SEGMENTS_TO_FETCH`] iterations and feeds real
    /// measured throughput back into the ABR controller.
    struct HttpPlaybackDriver {
        throughput_tracker: Arc<AtomicU64>,
        segments_fetched: u32,
        last_transfer_ms: f64,
    }

    #[async_trait(?Send)]
    impl PlaybackDriver for HttpPlaybackDriver {
        fn position_s(&self) -> f64 {
            0.0
        }

        fn should_continue(&self) -> bool {
            self.segments_fetched < SEGMENTS_TO_FETCH
        }

        fn on_iteration_start(&mut self, _session_pos: f64) {
            self.segments_fetched += 1;
        }

        fn throughput_override_bps(&self) -> Option<f64> {
            let raw = self.throughput_tracker.load(Ordering::Relaxed);
            if raw == 0 {
                None
            } else {
                Some(f64::from_bits(raw))
            }
        }

        fn on_iteration(&mut self, s: &IterationSummary<'_>) {
            self.last_transfer_ms = s.last_transfer_ms;

            let (vid_idx, vid_reason) = s
                .video_decision
                .map(|d| (d.index.to_string(), format!("{:?}", d.reason)))
                .unwrap_or_else(|| ("done".into(), "-".into()));
            let (aud_idx, aud_reason) = s
                .audio_decision
                .map(|d| (d.index.to_string(), format!("{:?}", d.reason)))
                .unwrap_or_else(|| ("done".into(), "-".into()));

            println!(
                "  {:<4}  {:<8}  {:<12}  {:<8}  {:<12}  {:>4} ms  ({:.0} kbps measured)",
                self.segments_fetched,
                vid_idx,
                vid_reason,
                aud_idx,
                aud_reason,
                s.last_transfer_ms as u64,
                s.last_throughput_bps / 1_000.0,
            );
        }
    }

    // 6. run() fetches until should_continue() returns false, then calls
    //    end_of_stream() and returns.
    let driver = HttpPlaybackDriver {
        throughput_tracker: Arc::clone(&throughput_tracker),
        segments_fetched: 0,
        last_transfer_ms: 0.0,
    };
    Player::new(session, driver).run().await?;

    println!("\nEnd of stream. Done.");

    Ok(())
}
