//! # dash-rs
//!
//! A Rust implementation of MPEG-DASH playback logic, inspired by
//! [dash.js](https://github.com/Dash-Industry-Forum/dash.js).
//!

#![cfg_attr(not(feature = "std"), no_std)]

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
pub mod throughput_cell;
mod time;
pub mod util;

#[cfg(not(feature = "std"))]
mod url_type;

#[cfg(feature = "std")]
pub use url::Url;
#[cfg(not(feature = "std"))]
pub use url_type::Url;

// Top-level re-exports for the most commonly used types.
pub use abr::rule::RepresentationInfo;
pub use abr::{AbrController, AbrDecisionReason, AbrMetrics, QualityDecision};
pub use clock::Clock;
#[cfg(feature = "std")]
pub use clock::SystemClock;
pub use error::DashError;
pub use fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher};
pub use manifest::MPD;
pub use media_buffer::{MediaBuffer, NullBuffer, StubBuffer, StubChunk, TimeRange};
pub use mp4::parse_all_emsg;
pub use mp4::{parse_prft, parse_sidx, EmsgInfo, ProducerReferenceTime, SidxInfo};
pub use player::Player;
pub use player::{
    DashEvent, PlayerEvent, PlayerIterationEvent, PlayerLogEvent, PlayerLogLevel,
    PlayerRunEndReason,
};
pub use session::driver::{IterationSummary, PlaybackDriver, PlaybackState};
pub use session::{
    BufferLevelSample, DashMetrics, DashSession, FetchOutcome, GapControllerSettings,
    GapSeekReason, GapSeekTarget, HttpRequestRecord, MediaType, RepresentationSwitchRecord,
};
pub use throughput_cell::ThroughputCell;
