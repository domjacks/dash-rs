//! Pluggable clock abstraction used by [`crate::session::DashSession`].
//!
//! The library calls [`Clock::now_ms`] for all monotonic time measurements
//! (buffer-level estimation, pause accounting, manifest refresh scheduling)
//! and [`Clock::unix_s`] for UTC wall-clock time (live-edge snapping, UTC
//! timing sync).
//!
//! [`Clock::set_timeout`] is used by the buffer-gate loop inside
//! [`crate::session::DashSession::wait_for_buffer_capacity`].  Each
//! implementation delegates to the platform's native async timer:
//!
//! | Type | `set_timeout` implementation |
//! |---|---|
//! | [`SystemClock`] | `futures_timer::Delay` (works with any executor; no tokio required) |
//! | `BridgeClock` (WASM player) | `JsFuture::from(bridge.sleep(ms))` |
//!
//! Two read-only implementations are provided:
//!
//! | Type | When to use |
//! |---|---|
//! | [`SystemClock`] | Native (desktop / embedded) — the default |
//! | Custom | WASM — provide a clock backed by `performance.now()` / `js_sys::Date::now()` |
//!
//! Inject a custom clock via [`crate::session::DashSession::from_mpd_with_clock`]
//! or [`crate::session::DashSession::from_url_with_clock`].

use alloc::boxed::Box;
use alloc::sync::Arc;
#[cfg(feature = "std")]
use std::time::Duration;

use async_trait::async_trait;

// ── Clock trait ───────────────────────────────────────────────────────────────

/// Abstraction over a time source and async timer.
///
/// Allows [`crate::session::DashSession`] to compile and run on platforms
/// where `std::time::Instant`, `SystemTime::now()`, and runtime-specific
/// sleep primitives are unavailable (e.g. `wasm32-unknown-unknown`).
///
/// Both time methods must be monotonically non-decreasing within a single
/// session lifetime; backward jumps will cause undefined buffer-level
/// behaviour.
///
/// # Implementing on WASM
///
/// WASM is single-threaded so implementors may declare `unsafe impl Send` /
/// `unsafe impl Sync` and use `JS` objects internally.  `set_timeout` should
/// delegate to `bridge.sleep(ms)` via `JsFuture`.
///
/// # Default methods
///
/// [`Clock::set_interval`] has a default implementation that delegates to
/// [`Clock::set_timeout`].  Override it only if you need more precise
/// drift-free interval behaviour (e.g. compensating for processing time).
#[async_trait(?Send)]
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

    /// Complete after `ms` milliseconds (one-shot timer).
    ///
    /// Used by [`crate::session::DashSession::wait_for_buffer_capacity`] to
    /// yield between buffer-level polls without blocking the async executor.
    async fn set_timeout(&self, ms: f64);

    /// Complete after one `ms`-millisecond interval tick.
    ///
    /// The default implementation delegates to [`Clock::set_timeout`].
    /// Call in a loop to drive periodic work (e.g. a gap-controller tick):
    ///
    /// ```ignore
    /// loop {
    ///     clock.set_interval(200.0).await;
    ///     session.tick_gap(position_s, &buffered, is_paused);
    /// }
    /// ```
    ///
    /// Override for drift-corrected interval behaviour if required.
    async fn set_interval(&self, ms: f64) {
        self.set_timeout(ms).await;
    }
}

// ── SystemClock ───────────────────────────────────────────────────────────────

/// Default [`Clock`] implementation backed by the system wall clock.
///
/// - [`Clock::now_ms`] measures elapsed time since this struct was created,
///   using [`std::time::Instant`].
/// - [`Clock::unix_s`] calls [`std::time::SystemTime::now()`].
/// - [`Clock::set_timeout`] uses [`futures_timer::Delay`], which works with
///   any async executor without requiring tokio.
#[cfg(feature = "std")]
pub struct SystemClock {
    start: std::time::Instant,
}

#[cfg(feature = "std")]
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

#[cfg(feature = "std")]
impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "std")]
#[async_trait(?Send)]
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

    async fn set_timeout(&self, ms: f64) {
        futures_timer::Delay::new(Duration::from_millis(ms as u64)).await;
    }
}
