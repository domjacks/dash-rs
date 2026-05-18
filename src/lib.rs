//! # dash-rs
//!
//! A Rust implementation of MPEG-DASH playback logic, inspired by
//! [dash.js](https://github.com/Dash-Industry-Forum/dash.js).
//!
//! ## Quick start
//!
//! ```rust,ignore
//! use async_trait::async_trait;
//! use dash_rs::{
//!     fetch::stub::StubSegmentFetcher, media_buffer::NullBuffer,
//!     session::driver::PlaybackDriver, DashError, FetchOutcome, MediaType, Player,
//! };
//! use url::Url;
//!
//! struct MyDriver;
//!
//! #[async_trait(?Send)]
//! impl PlaybackDriver for MyDriver {
//!     fn position_s(&self) -> f64 { 0.0 }
//! }
//!
//! # async fn example() -> Result<(), DashError> {
//! let url = Url::parse("https://example.com/manifest.mpd")?;
//! Player::from_url(&url, StubSegmentFetcher, NullBuffer, MyDriver)
//!     .await?
//!     .run()
//!     .await?;
//! # Ok(())
//! # }
//! ```

#![cfg_attr(not(feature = "std"), no_std)]

#[macro_use]
extern crate alloc;

pub mod abr;
pub mod clock;
pub mod error;
pub mod fetch;
pub mod manifest;
pub(crate) mod math;
pub mod media_buffer;
pub mod mp4;
pub mod player;
pub mod session;
mod time;
pub mod util;

#[cfg(not(feature = "std"))]
mod url_type;

#[cfg(feature = "std")]
pub use url::Url;
#[cfg(not(feature = "std"))]
pub use url_type::Url;

// Top-level re-exports for the most commonly used types.
pub use abr::{AbrController, AbrDecisionReason, AbrMetrics, QualityDecision};
pub use abr::rule::RepresentationInfo;
pub use clock::Clock;
#[cfg(feature = "std")]
pub use clock::SystemClock;
pub use error::DashError;
pub use fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher};
pub use manifest::MPD;
pub use media_buffer::{MediaBuffer, NullBuffer, StubBuffer, StubChunk, TimeRange};
pub use mp4::{parse_prft, parse_sidx, EmsgInfo, ProducerReferenceTime, SidxInfo};
#[cfg(feature = "events")]
pub use mp4::parse_all_emsg;
pub use player::Player;
pub use session::driver::{IterationSummary, PlaybackDriver, PlaybackState};
#[cfg(feature = "events")]
pub use session::DashEvent;
pub use session::{
    BufferLevelSample, DashMetrics, DashSession, FetchOutcome, GapControllerSettings,
    GapSeekReason, GapSeekTarget, HttpRequestRecord, MediaType, RepresentationSwitchRecord,
};
