//! [`Player`] — a convenience wrapper that owns a [`DashSession`], a
//! [`MediaBuffer`], and a [`PlaybackDriver`] together.
//!
//! # Motivation
//!
//! Without `Player`, callers must construct and hold three separate values:
//!
//! ```rust,ignore
//! let session = DashSession::from_url(&url, fetcher).await?;
//! let mut buf = NullBuffer;
//! let mut driver = MyDriver::new();
//! session.run(&mut buf, &mut driver).await?;
//! ```
//!
//! `Player` folds these into a single storable type with a simpler call site:
//!
//! ```rust,ignore
//! let mut player = Player::from_url(&url, fetcher, NullBuffer, driver).await?;
//! player.run().await?;
//! ```
//!
//! Or as a one-liner:
//!
//! ```rust,ignore
//! Player::from_url(&url, fetcher, NullBuffer, MyDriver::new()).await?.run().await?;
//! ```
//!
//! # Choosing a constructor
//!
//! | Situation | Constructor |
//! |---|---|
//! | URL is known at construction time | [`Self::from_url`] (or `from_url_with_clock` on WASM) |
//! | Manifest already parsed (e.g. stub / unit test) | [`Self::from_mpd`] (or `from_mpd_with_clock` without `std`) |
//!
//! All constructors accept the buffer and driver as the final arguments.  If
//! you need to inspect session state (e.g. available representations) before
//! starting the loop, use the [`session`][Self::session] accessor after
//! construction.
//!
//! # Accessing session, buffer, or driver state after `run()`
//!
//! All three halves remain accessible through the public accessors:
//!
//! ```rust,ignore
//! let metrics = player.session().metrics();
//! let last_ms = player.driver().last_transfer_ms;
//! let chunks  = player.buffer().video_chunks.len();
//! ```

use alloc::sync::Arc;

use crate::{
    abr::rule::RepresentationInfo,
    clock::Clock,
    error::DashError,
    fetch::SegmentFetcher,
    manifest::MPD,
    media_buffer::MediaBuffer,
    session::{driver::PlaybackDriver, DashSession},
    Url,
};

// ── Player ────────────────────────────────────────────────────────────────────

/// A single owned value that combines a [`DashSession`], a [`MediaBuffer`],
/// and a [`PlaybackDriver`].
///
/// See the [module documentation][self] for usage examples.
pub struct Player<F: SegmentFetcher, S: MediaBuffer, D> {
    session: DashSession<F>,
    buf: S,
    driver: D,
}

impl<F: SegmentFetcher, S: MediaBuffer, D: PlaybackDriver> Player<F, S, D> {
    // ── Constructors ──────────────────────────────────────────────────────────

    /// Fetch and parse the manifest at `url` and return a `Player` ready to
    /// [`run`][Self::run].
    ///
    /// Uses a [`SystemClock`][crate::clock::SystemClock] for time measurements.
    /// Call [`Self::from_url_with_clock`] to inject a custom clock (e.g. on
    /// WASM where `std::time::Instant` is unavailable).
    #[cfg(feature = "std")]
    pub async fn from_url(url: &Url, fetcher: F, buf: S, driver: D) -> Result<Self, DashError> {
        let session = DashSession::from_url(url, fetcher).await?;
        Ok(Self {
            session,
            buf,
            driver,
        })
    }

    /// Like [`Self::from_url`] but accepts a custom [`Clock`] implementation.
    ///
    /// Use this on platforms where `std::time::Instant` / `SystemTime::now()`
    /// are unavailable (e.g. `wasm32-unknown-unknown`).
    pub async fn from_url_with_clock(
        url: &Url,
        fetcher: F,
        buf: S,
        driver: D,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DashError> {
        let session = DashSession::from_url_with_clock(url, fetcher, clock).await?;
        Ok(Self {
            session,
            buf,
            driver,
        })
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
        buf: S,
        driver: D,
    ) -> Result<Self, DashError> {
        let session = DashSession::from_mpd(mpd, base_url, fetcher)?;
        Ok(Self {
            session,
            buf,
            driver,
        })
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
        buf: S,
        driver: D,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DashError> {
        let session = DashSession::from_mpd_with_clock(mpd, base_url, fetcher, clock)?;
        Ok(Self {
            session,
            buf,
            driver,
        })
    }

    // ── Playback ──────────────────────────────────────────────────────────────

    /// Run the DASH fetch loop to completion (or until
    /// [`PlaybackDriver::should_continue`] returns `false`).
    ///
    /// Delegates directly to [`DashSession::run`].  The full startup sequence
    /// (`open` → `set_live_mode` → `add_track` → init segments → PTS remap →
    /// `on_session_started` → `notify_playing`) is performed inside `run()`
    /// before the fetch loop begins.
    pub async fn run(&mut self) -> Result<(), DashError> {
        self.session.run(&mut self.buf, &mut self.driver).await
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// Immutable reference to the inner [`DashSession`].
    ///
    /// Use this after construction to inspect available representations before
    /// calling [`run()`][Self::run], or after `run()` to read metrics.
    pub fn session(&self) -> &DashSession<F> {
        &self.session
    }

    /// Mutable reference to the inner [`DashSession`].
    ///
    /// Allows callers to call `seek_to`, `set_buffer_target_ms`, etc. between
    /// iterations when driving the session manually alongside `run()`.
    pub fn session_mut(&mut self) -> &mut DashSession<F> {
        &mut self.session
    }

    /// Immutable reference to the inner [`MediaBuffer`].
    pub fn buffer(&self) -> &S {
        &self.buf
    }

    /// Mutable reference to the inner [`MediaBuffer`].
    pub fn buffer_mut(&mut self) -> &mut S {
        &mut self.buf
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

    // ── Convenience forwarders ────────────────────────────────────────────────

    /// Reposition the session's segment-fetch cursor to the nearest segment
    /// boundary at or before `position_s`.
    ///
    /// Forwards to [`DashSession::seek_to`].  Returns the snapped position.
    pub fn seek_to(&mut self, position_s: f64) -> f64 {
        self.session.seek_to(position_s)
    }

    /// Returns `true` when the manifest is a live stream (`MPD@type="dynamic"`).
    ///
    /// Forwards to [`DashSession::is_live`].
    pub fn is_live(&self) -> bool {
        self.session.is_live()
    }

    /// Total presentation duration in seconds, or `None` for live streams.
    ///
    /// Forwards to [`DashSession::duration_s`].
    pub fn duration_s(&self) -> Option<f64> {
        self.session.duration_s()
    }

    /// Available video representations sorted ascending by bandwidth, or an
    /// empty slice if the stream has no video track.
    ///
    /// Forwards to [`DashSession::video_representations`].
    pub fn video_representations(&self) -> &[RepresentationInfo] {
        self.session.video_representations()
    }

    /// Available audio representations sorted ascending by bandwidth, or an
    /// empty slice if the stream has no audio track.
    ///
    /// Forwards to [`DashSession::audio_representations`].
    pub fn audio_representations(&self) -> &[RepresentationInfo] {
        self.session.audio_representations()
    }

    // ── Destructuring ─────────────────────────────────────────────────────────

    /// Consume the `Player` and return the inner `(session, buf, driver)` triple.
    ///
    /// Useful when ownership of all three halves is needed after playback ends.
    pub fn into_parts(self) -> (DashSession<F>, S, D) {
        (self.session, self.buf, self.driver)
    }
}
