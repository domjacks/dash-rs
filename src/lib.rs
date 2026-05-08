//! # dash-rs
//!
//! A Rust implementation of MPEG-DASH playback logic, inspired by
//! [dash.js](https://github.com/Dash-Industry-Forum/dash.js).
//!
//! ## What this crate provides
//!
//! | Module | Responsibility | dash.js equivalent |
//! |---|---|---|
//! | [`abr`] | Adaptive bitrate rules and controller | `src/streaming/rules/` |
//! | [`media_source`] | MSE-like `MediaSource` / `SourceBuffer` traits + stub | `ExternalMediaSource.js`, `ExternalSourceBuffer.js`, `SourceBufferSink.js` |
//! | [`fetch`] | `SegmentFetcher` trait + stub | `FragmentLoader.js`, `URLLoader.js` |
//! | [`manifest`] | Manifest loading via `dash-mpd` | `ManifestLoader.js` |
//! | [`session`] | Top-level `DashSession` coordinator | `MediaPlayer.js`, `StreamProcessor.js` |
//!
//! ## Quick start
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use url::Url;
//! # use dash_rs::session::{DashSession, MediaType};
//! # use dash_rs::fetch::stub::StubSegmentFetcher;
//! # use dash_rs::media_source::stub::StubMediaSource;
//! # use dash_rs::abr::AbrMetrics;
//! # #[tokio::main]
//! # async fn main() -> Result<(), dash_rs::error::DashError> {
//! // 1. Create (and open) a media sink.
//! let mut media_source = StubMediaSource::new();
//! media_source.open();
//!
//! // 2. Load the manifest and start a session.
//! let url = Url::parse("https://rdmedia.bbc.co.uk/bbb/2/client_manifest-common_init.mpd").unwrap();
//! let mut session = DashSession::from_url(&url, StubSegmentFetcher, media_source).await?;
//!
//! // 3. Provide real throughput / buffer measurements; use zeroed placeholders here.
//! let metrics = AbrMetrics::new(5_000_000.0, 8_000.0); // 5 Mbps, 8 s buffer
//!
//! // 4. Fetch the next video segment — ABR selects the quality automatically.
//! let decision = session.fetch_next_segment(MediaType::Video, &metrics).await?;
//! println!("fetched at quality index {}", decision.index);
//! # Ok(())
//! # }
//! ```

pub mod abr;
pub mod clock;
pub mod error;
pub mod fetch;
pub mod manifest;
pub mod media_source;
pub mod mp4;
pub mod session;
pub mod util;
mod time;

// Top-level re-exports for the most commonly used types.
pub use abr::{AbrController, AbrMetrics, QualityDecision};
pub use clock::{Clock, SystemClock};
pub use error::DashError;
pub use manifest::MPD;
pub use mp4::{parse_all_emsg, parse_sidx, EmsgInfo, SidxInfo};
pub use session::{DashEvent, DashSession, GapControllerSettings, GapSeekReason, GapSeekTarget, MediaType};
