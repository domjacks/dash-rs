//! Basic usage example.
//!
//! Demonstrates constructing a `DashSession` from a pre-built MPD string,
//! running the ABR controller, and simulating a fetch + append loop using
//! the stub implementations — no real network traffic occurs.
//!
//! Run with:
//!   cargo run --example basic

use std::sync::Arc;

use async_trait::async_trait;
use dash_rs::{
    fetch::stub::StubSegmentFetcher,
    manifest::MPD,
    media_source::stub::StubMediaSource,
    session::{driver::{IterationSummary, PlaybackDriver}, DashSession},
    DashError, Player,
};
use url::Url;

/// A minimal, synthetically constructed MPD string that mirrors a real-world
/// multi-bitrate video manifest. We build it in code so the example runs
/// without network access.
const EXAMPLE_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     profiles="urn:mpeg:dash:profile:isoff-on-demand:2011"
     type="static"
     mediaPresentationDuration="PT60S"
     minBufferTime="PT2S">
  <Period id="1" duration="PT60S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="video/$RepresentationID$/seg-$Number$.m4s"
                       initialization="video/$RepresentationID$/init.mp4"
                       startNumber="1" duration="4" timescale="1"/>
      <Representation id="v-low"  bandwidth="500000"  width="640"  height="360"  codecs="avc1.42E01E"/>
      <Representation id="v-mid"  bandwidth="1500000" width="1280" height="720"  codecs="avc1.4D401F"/>
      <Representation id="v-high" bandwidth="4000000" width="1920" height="1080" codecs="avc1.640028"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio">
      <SegmentTemplate media="audio/$RepresentationID$/seg-$Number$.m4s"
                       initialization="audio/$RepresentationID$/init.mp4"
                       startNumber="1" duration="4" timescale="1"/>
      <Representation id="a-low"  bandwidth="64000"  codecs="mp4a.40.2"/>
      <Representation id="a-high" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

#[tokio::main]
async fn main() -> Result<(), DashError> {
    // Initialise tracing so all debug/info events from the library are visible.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    // 1. Create a stub media source (replaces the browser's MediaSource API).
    //    `StubMediaSource` starts as Open by default — no explicit `open()`
    //    call is needed.
    let media_source = StubMediaSource::new();

    // 2. Parse the MPD and construct a session directly from the parsed MPD.
    //    In a real application you would call `DashSession::from_url` instead.
    let base_url = Url::parse("https://example.com/").unwrap();
    let mpd: MPD = dash_rs::manifest::parse(EXAMPLE_MPD).map_err(DashError::ManifestParse)?;

    let fetcher = Arc::new(StubSegmentFetcher);
    let session = DashSession::from_mpd(mpd, base_url, fetcher, media_source)?;

    // 3. Inspect available representations.
    println!("=== Video representations ===");
    for (i, rep) in session.video_representations().iter().enumerate() {
        println!(
            "  [{i}] id={} bandwidth={}kbps {}x{}",
            rep.id,
            rep.bandwidth / 1000,
            rep.width.unwrap_or(0),
            rep.height.unwrap_or(0),
        );
    }
    println!("=== Audio representations ===");
    for (i, rep) in session.audio_representations().iter().enumerate() {
        println!(
            "  [{i}] id={} bandwidth={}kbps",
            rep.id,
            rep.bandwidth / 1000
        );
    }

    // 4. Simulate a short fetch loop — 5 segments per track — via PlaybackDriver.
    //    Throughput escalates each iteration so the ABR controller should step
    //    up quality over time.
    println!("\n=== Fetching segments ===");

    /// [`PlaybackDriver`] for the basic stub example.
    ///
    /// Provides an escalating synthetic throughput so the ABR controller
    /// steps up representation quality over the five iterations.
    struct BasicPlaybackDriver {
        /// 1-based iteration counter; 0 before the first `on_iteration_start`.
        seg: u32,
    }

    #[async_trait(?Send)]
    impl PlaybackDriver for BasicPlaybackDriver {
        fn position_s(&self) -> f64 {
            (self.seg as f64) * 4.0
        }

        fn should_continue(&self) -> bool {
            self.seg < 5
        }

        fn on_iteration_start(&mut self, _pos: f64) {
            self.seg += 1;
        }

        fn throughput_override_bps(&self) -> Option<f64> {
            Some(1_200_000.0 + (self.seg as f64) * 400_000.0)
        }

        fn on_iteration(&mut self, s: &IterationSummary<'_>) {
            let vid = s
                .video_decision
                .map(|d| (d.index, format!("{:?}", d.reason)));
            let aud = s
                .audio_decision
                .map(|d| (d.index, format!("{:?}", d.reason)));
            println!(
                "  seg {}: video quality={} ({:?}), audio quality={} ({:?})",
                self.seg,
                vid.as_ref()
                    .map(|(i, _)| i.to_string())
                    .unwrap_or_else(|| "done".into()),
                vid.as_ref().map(|(_, r)| r.as_str()).unwrap_or("-"),
                aud.as_ref()
                    .map(|(i, _)| i.to_string())
                    .unwrap_or_else(|| "done".into()),
                aud.as_ref().map(|(_, r)| r.as_str()).unwrap_or("-"),
            );
        }
    }

    let mut player = Player::new(session, BasicPlaybackDriver { seg: 0 });
    player.run().await?;

    println!("\nEnd of stream signalled. Done.");

    Ok(())
}
