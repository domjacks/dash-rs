//! [`ThroughputCell`] — a shareable, atomic throughput register.
//!
//! Encapsulates the `Arc<AtomicU64>` + `f64::to_bits` / `from_bits` pattern
//! that fetcher and driver implementations use to share a measured bandwidth
//! value without locking.
//!
//! # Usage
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use dash_rs::ThroughputCell;
//!
//! // Share one cell between the fetcher and driver.
//! let cell = Arc::new(ThroughputCell::new());
//!
//! // Inside the fetcher — record the measured bitrate after each download:
//! cell.set(bytes_received as f64 * 8.0 / elapsed_s);
//!
//! // Inside the driver — feed the last measurement to the ABR controller:
//! fn throughput_override_bps(&self) -> Option<f64> {
//!     self.cell.get()
//! }
//! ```

use core::sync::atomic::{AtomicU64, Ordering};

/// An atomic cell that stores a single `f64` throughput value (bits per second).
///
/// Uses `0` as the sentinel "no data yet" value — store a positive measured
/// bitrate with [`set`][Self::set] and read it back with [`get`][Self::get].
///
/// Designed to be wrapped in an `Arc` and shared between a [`SegmentFetcher`]
/// and a [`PlaybackDriver`].
///
/// [`SegmentFetcher`]: crate::fetch::SegmentFetcher
/// [`PlaybackDriver`]: crate::session::driver::PlaybackDriver
pub struct ThroughputCell(AtomicU64);

impl ThroughputCell {
    /// Create a new cell with no data (sentinel value `0`).
    pub fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Store `bps` (bits per second) in the cell.
    ///
    /// Values of `0.0`, `NaN`, and `±Infinity` are stored as-is; callers
    /// should avoid publishing meaningless measurements.  A positive measured
    /// bitrate is the only value that makes sense to pass here.
    pub fn set(&self, bps: f64) {
        self.0.store(bps.to_bits(), Ordering::Relaxed);
    }

    /// Return the last stored throughput, or `None` if no measurement has been
    /// recorded yet (the cell holds the sentinel value `0`).
    pub fn get(&self) -> Option<f64> {
        let raw = self.0.load(Ordering::Relaxed);
        if raw == 0 {
            None
        } else {
            Some(f64::from_bits(raw))
        }
    }
}

impl Default for ThroughputCell {
    fn default() -> Self {
        Self::new()
    }
}
