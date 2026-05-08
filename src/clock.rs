//! Pluggable clock abstraction used by [`crate::session::DashSession`].
//!
//! The library calls [`Clock::now_ms`] for all monotonic time measurements
//! (buffer-level estimation, pause accounting, manifest refresh scheduling)
//! and [`Clock::unix_s`] for UTC wall-clock time (live-edge snapping, UTC
//! timing sync).
//!
//! Two implementations are provided:
//!
//! | Type | When to use |
//! |---|---|
//! | [`SystemClock`] | Native (desktop / embedded) — the default |
//! | Custom | WASM — provide a clock backed by `performance.now()` / `js_sys::Date::now()` |
//!
//! Inject a custom clock via [`crate::session::DashSession::from_mpd_with_clock`]
//! or [`crate::session::DashSession::from_url_with_clock`].

use std::sync::Arc;

// ── Clock trait ───────────────────────────────────────────────────────────────

/// Abstraction over a time source.
///
/// Allows [`crate::session::DashSession`] to compile and run on platforms
/// where `std::time::Instant` and `SystemTime::now()` are unavailable (e.g.
/// `wasm32-unknown-unknown`).
///
/// Both methods must be monotonically non-decreasing within a single session
/// lifetime; backward jumps will cause undefined buffer-level behaviour.
pub trait Clock: Send + Sync {
    /// Monotonic elapsed time in milliseconds.
    ///
    /// Used for buffer-level estimation, pause accounting, and manifest
    /// refresh scheduling.  The absolute value is arbitrary — only deltas
    /// between two calls matter.
    fn now_ms(&self) -> f64;

    /// Current UTC time as seconds since the Unix epoch.
    ///
    /// Used for live-edge segment snapping and `UTCTiming` offset
    /// calculations.  Returns `0.0` on error.
    fn unix_s(&self) -> f64;
}

// ── SystemClock ───────────────────────────────────────────────────────────────

/// Default [`Clock`] implementation backed by the system wall clock.
///
/// - [`Clock::now_ms`] measures elapsed time since this struct was created,
///   using [`std::time::Instant`].
/// - [`Clock::unix_s`] calls [`std::time::SystemTime::now()`].
pub struct SystemClock {
    start: std::time::Instant,
}

impl SystemClock {
    /// Create a new clock anchored at the current instant.
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }

    /// Convenience constructor that wraps `self` in an `Arc`.
    pub fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1_000.0
    }

    fn unix_s(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }
}
