//! [`Player`] — a convenience wrapper that owns a [`DashSession`] and a
//! [`PlaybackDriver`] together.
//!
//! # Motivation
//!
//! Without `Player`, callers must construct and hold two separate values:
//!
//! ```rust,ignore
//! let session = DashSession::from_url(&url, fetcher, media_source).await?;
//! let mut driver = MyDriver::new(/* session-independent args */);
//! session.run(&mut driver).await?;
//! ```
//!
//! `Player` folds these into a single storable type with a simpler call site:
//!
//! ```rust,ignore
//! let mut player = Player::from_url(&url, fetcher, media_source, driver).await?;
//! player.run().await?;
//! ```
//!
//! Or as a one-liner:
//!
//! ```rust,ignore
//! Player::from_url(&url, fetcher, media_source, MyDriver::new()).await?.run().await?;
//! ```
//!
//! # Choosing a constructor
//!
//! | Situation | Constructor |
//! |---|---|
//! | URL is known at construction time | [`Self::from_url`] (or `from_url_with_clock` on WASM) |
//! | Manifest already parsed (e.g. stub / unit test) | [`Self::from_mpd`] (or `from_mpd_with_clock` without `std`) |
//!
//! All constructors accept the driver as the final argument.  If you need to
//! inspect session state (e.g. available representations) before starting the
//! loop, use the [`session`][Self::session] accessor after construction.
//!
//! # Accessing session or driver state after `run()`
//!
//! Both halves remain accessible through the public accessors:
//!
//! ```rust,ignore
//! let metrics = player.session().metrics();
//! let last_ms = player.driver().last_transfer_ms;
//! ```

use alloc::sync::Arc;

use crate::{
    clock::Clock,
    error::DashError,
    fetch::SegmentFetcher,
    manifest::MPD,
    media_source::MediaSource,
    session::{driver::PlaybackDriver, DashSession},
    Url,
};

// ── Player ────────────────────────────────────────────────────────────────────

/// A single owned value that combines a [`DashSession`] with a
/// [`PlaybackDriver`].
///
/// See the [module documentation][self] for usage examples.
pub struct Player<F: SegmentFetcher, M: MediaSource, D> {
    session: DashSession<F, M>,
    driver: D,
}

impl<F: SegmentFetcher, M: MediaSource, D: PlaybackDriver> Player<F, M, D> {
    // ── Constructors ──────────────────────────────────────────────────────────

    /// Fetch and parse the manifest at `url`, open the media source, and return
    /// a `Player` ready to [`run`][Self::run].
    ///
    /// Uses a [`SystemClock`][crate::clock::SystemClock] for time measurements.
    /// Call [`Self::from_url_with_clock`] to inject a custom clock (e.g. on
    /// WASM where `std::time::Instant` is unavailable).
    #[cfg(feature = "std")]
    pub async fn from_url(
        url: &Url,
        fetcher: F,
        media_source: M,
        driver: D,
    ) -> Result<Self, DashError> {
        let session = DashSession::from_url(url, fetcher, media_source).await?;
        Ok(Self { session, driver })
    }

    /// Like [`Self::from_url`] but accepts a custom [`Clock`] implementation.
    ///
    /// Use this on platforms where `std::time::Instant` / `SystemTime::now()`
    /// are unavailable (e.g. `wasm32-unknown-unknown`).
    pub async fn from_url_with_clock(
        url: &Url,
        fetcher: F,
        media_source: M,
        driver: D,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DashError> {
        let session =
            DashSession::from_url_with_clock(url, fetcher, media_source, clock).await?;
        Ok(Self { session, driver })
    }

    /// Wrap an already-parsed `mpd` into a `Player` ready to
    /// [`run`][Self::run].
    ///
    /// This is the preferred entry point when the manifest is already in
    /// memory (e.g. stub / unit-test scenarios where no network fetch is
    /// needed).  For URL-based construction use [`Self::from_url`].
    ///
    /// Uses a [`SystemClock`][crate::clock::SystemClock] for time measurements.
    /// Call [`Self::from_mpd_with_clock`] to inject a custom clock.
    #[cfg(feature = "std")]
    pub fn from_mpd(
        mpd: MPD,
        base_url: Url,
        fetcher: Arc<F>,
        media_source: M,
        driver: D,
    ) -> Result<Self, DashError> {
        let session = DashSession::from_mpd(mpd, base_url, fetcher, media_source)?;
        Ok(Self { session, driver })
    }

    /// Like [`Self::from_mpd`] but accepts a custom [`Clock`] implementation.
    ///
    /// Use this on platforms where `std::time::Instant` / `SystemTime::now()`
    /// are unavailable (e.g. `wasm32-unknown-unknown` without the `std`
    /// feature).
    pub fn from_mpd_with_clock(
        mpd: MPD,
        base_url: Url,
        fetcher: Arc<F>,
        media_source: M,
        driver: D,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DashError> {
        let session =
            DashSession::from_mpd_with_clock(mpd, base_url, fetcher, media_source, clock)?;
        Ok(Self { session, driver })
    }

    // ── Playback ──────────────────────────────────────────────────────────────

    /// Run the DASH fetch loop to completion (or until
    /// [`PlaybackDriver::should_continue`] returns `false`).
    ///
    /// Delegates directly to [`DashSession::run`].  The full startup sequence
    /// (`open` → `set_live_mode` → init segments → PTS remap →
    /// `on_session_started` → `notify_playing`) is performed inside `run()`
    /// before the fetch loop begins.
    pub async fn run(&mut self) -> Result<(), DashError> {
        self.session.run(&mut self.driver).await
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// Immutable reference to the inner [`DashSession`].
    ///
    /// Use this after construction to inspect available representations before
    /// calling [`run()`][Self::run], or after `run()` to read metrics.
    pub fn session(&self) -> &DashSession<F, M> {
        &self.session
    }

    /// Mutable reference to the inner [`DashSession`].
    ///
    /// Allows callers to call `seek_to`, `set_buffer_target_ms`, etc. between
    /// iterations when driving the session manually alongside `run()`.
    pub fn session_mut(&mut self) -> &mut DashSession<F, M> {
        &mut self.session
    }

    /// Immutable reference to the inner [`PlaybackDriver`].
    ///
    /// Use this after [`run()`][Self::run] to read driver-side state such as
    /// accumulated segment counters or the last measured transfer time.
    pub fn driver(&self) -> &D {
        &self.driver
    }

    /// Mutable reference to the inner [`PlaybackDriver`].
    pub fn driver_mut(&mut self) -> &mut D {
        &mut self.driver
    }

    // ── Destructuring ─────────────────────────────────────────────────────────

    /// Consume the `Player` and return the inner `(session, driver)` pair.
    ///
    /// Useful when ownership of both halves is needed after playback ends.
    pub fn into_parts(self) -> (DashSession<F, M>, D) {
        (self.session, self.driver)
    }
}
