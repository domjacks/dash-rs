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
use alloc::{boxed::Box, format, rc::Rc, string::String, vec::Vec};
use core::{cell::Cell, cell::RefCell};

use async_trait::async_trait;
use bytes::Bytes;

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

use crate::{
    abr::AbrDecisionReason,
    media_buffer::TimeRange,
    session::{driver::PlaybackState, GapSeekTarget, MediaType},
};

use crate::mp4::parse_all_emsg;
use crate::settings::PlayerSettings;

type EventHandler = Box<dyn FnMut(&PlayerEvent)>;
type EventHandlers = Rc<RefCell<Vec<EventHandler>>>;

// ── PlayerEvent ───────────────────────────────────────────────────────────────

/// An in-band DASH event extracted from a `DASHEventMessageBox` (`emsg`) in a
/// media segment appended through [`Player`].
///
/// # Presentation time semantics
///
/// | `emsg` version | `presentation_time_s` meaning |
/// |---|---|
/// | 0 | Seconds relative to the segment's earliest presentation time.  Add `sidx.start_s` (if available) to get an absolute timeline position. |
/// | 1 | Absolute seconds from the start of the media presentation timeline. |
#[derive(Debug, Clone, PartialEq)]
pub struct DashEvent {
    /// Which media track's segment contained this event.
    pub media_type: MediaType,
    /// URI identifying the event scheme (e.g. `"urn:scte:scte35:2014:xml+bin"`).
    pub scheme_id_uri: String,
    /// Optional value qualifying the scheme (empty string when absent).
    pub value: String,
    /// Presentation time in seconds.
    pub presentation_time_s: f64,
    /// Duration of the event in seconds.
    pub duration_s: f64,
    /// Unique identifier for this event within its scheme.
    pub id: u32,
    /// Raw event message payload bytes.
    pub message_data: Bytes,
}

/// Owned per-iteration playback snapshot emitted by [`PlayerEvent::Iteration`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerIterationEvent {
    /// Session estimated playback position at iteration start (s).
    pub position_s: f64,
    /// Video quality decision index for this iteration, if video was fetched.
    pub video_index: Option<usize>,
    /// Audio quality decision index for this iteration, if audio was fetched.
    pub audio_index: Option<usize>,
    /// Declared bandwidth (bps) of the active video representation.
    pub video_bandwidth: u64,
    /// Declared bandwidth (bps) of the active audio representation.
    pub audio_bandwidth: u64,
    /// Estimated video buffer level at iteration end (ms).
    pub video_buffer_ms: f64,
    /// Estimated audio buffer level at iteration end (ms).
    pub audio_buffer_ms: f64,
    /// Session buffer target (ms).
    pub buffer_target_ms: f64,
    /// Total HTTP request records accumulated by the session.
    pub http_request_count: usize,
    /// Total representation-switch records accumulated by the session.
    pub switch_count: usize,
    /// URL of the most recently completed HTTP request, or empty when absent.
    pub last_transfer_url: String,
    /// Transfer duration of the most recently completed HTTP request (ms).
    pub last_transfer_ms: f64,
    /// Measured throughput of the most recently completed HTTP request (bps).
    pub last_throughput_bps: Option<f64>,
    /// Total video representations available in the current period.
    pub video_rep_count: u32,
    /// Total audio representations available in the current period.
    pub audio_rep_count: u32,
    /// Whether the session is live.
    pub is_live: bool,
}

/// Severity for [`PlayerLogEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerLogLevel {
    /// Verbose diagnostics.
    Debug,
    /// Informational state transition.
    Info,
    /// Recoverable condition callers may want to surface.
    Warn,
    /// Error condition emitted before `run` returns an error.
    Error,
}

/// Consumer-facing log message emitted by [`Player`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerLogEvent {
    /// Message severity.
    pub level: PlayerLogLevel,
    /// Static subsystem name.
    pub target: &'static str,
    /// Human-readable message.
    pub message: String,
}

/// Why a [`Player`] run loop ended successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerRunEndReason {
    /// All active tracks reached natural end-of-stream.
    EndOfStream,
    /// The playback driver requested loop termination via
    /// [`PlaybackDriver::should_continue`].
    StoppedByDriver,
}

/// Events emitted by [`Player`] while it runs.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// Consumer-facing log message.
    Log(PlayerLogEvent),
    /// Per-iteration playback and transfer snapshot.
    Iteration(PlayerIterationEvent),
    /// In-band DASH metadata from an `emsg` box in an appended media segment.
    InbandDash(DashEvent),
    /// The renderer playback state changed between observed run-loop samples.
    PlaybackStateChanged {
        /// Previous observed playback state.
        from: PlaybackState,
        /// New observed playback state.
        to: PlaybackState,
    },
    /// The ABR controller switched to a different representation.
    QualityChange {
        /// Track whose quality changed.
        media_type: MediaType,
        /// Previous representation index.
        old_index: usize,
        /// Newly selected representation index.
        new_index: usize,
        /// Why the ABR controller selected the new index.
        reason: AbrDecisionReason,
    },
    /// The gap controller requested a renderer seek.
    GapSeek(GapSeekTarget),
    /// The session startup sequence completed.
    SessionStarted {
        /// Initial presentation position in seconds.
        start_s: f64,
    },
    /// The run loop ended successfully.
    RunEnded {
        /// Why the loop ended.
        reason: PlayerRunEndReason,
    },
    /// The run loop ended after all active tracks reached natural EOS.
    EndOfStream,
}

fn emit_event(handlers: &EventHandlers, event: PlayerEvent) {
    for handler in handlers.borrow_mut().iter_mut() {
        handler(&event);
    }
}

fn emit_log(
    handlers: &EventHandlers,
    level: PlayerLogLevel,
    target: &'static str,
    message: impl Into<String>,
) {
    emit_event(
        handlers,
        PlayerEvent::Log(PlayerLogEvent {
            level,
            target,
            message: message.into(),
        }),
    );
}

fn new_event_handlers() -> EventHandlers {
    Rc::new(RefCell::new(Vec::new()))
}

// ── Player ────────────────────────────────────────────────────────────────────

/// A single owned value that combines a [`DashSession`], a [`MediaBuffer`],
/// and a [`PlaybackDriver`].
///
/// See the [module documentation][self] for usage examples.
pub struct Player<F: SegmentFetcher, S: MediaBuffer, D> {
    session: DashSession<F>,
    buf: S,
    driver: D,
    event_handlers: EventHandlers,
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
        Ok(Self::from_session(session, buf, driver))
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
        Self::from_url_with_settings(url, fetcher, buf, driver, clock, PlayerSettings::default())
            .await
    }

    /// Like [`Self::from_url_with_clock`] but tuned by `settings`.
    pub async fn from_url_with_settings(
        url: &Url,
        fetcher: F,
        buf: S,
        driver: D,
        clock: Arc<dyn Clock>,
        settings: PlayerSettings,
    ) -> Result<Self, DashError> {
        let session = DashSession::from_url_with_settings(url, fetcher, clock, settings).await?;
        Ok(Self::from_session(session, buf, driver))
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
        Ok(Self::from_session(session, buf, driver))
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
        Self::from_mpd_with_settings(
            mpd,
            base_url,
            fetcher,
            buf,
            driver,
            clock,
            PlayerSettings::default(),
        )
    }

    /// Like [`Self::from_mpd_with_clock`] but tuned by `settings`.
    pub fn from_mpd_with_settings(
        mpd: MPD,
        base_url: Url,
        fetcher: Arc<F>,
        buf: S,
        driver: D,
        clock: Arc<dyn Clock>,
        settings: PlayerSettings,
    ) -> Result<Self, DashError> {
        let session = DashSession::from_mpd_with_settings(mpd, base_url, fetcher, clock, settings)?;
        Ok(Self::from_session(session, buf, driver))
    }

    fn from_session(session: DashSession<F>, buf: S, driver: D) -> Self {
        Self {
            session,
            buf,
            driver,
            event_handlers: new_event_handlers(),
        }
    }

    // ── Playback ──────────────────────────────────────────────────────────────

    /// Run the DASH fetch loop to completion (or until
    /// [`PlaybackDriver::should_continue`] returns `false`).
    ///
    /// Delegates directly to [`DashSession::run`].  The full startup sequence
    /// (`open` → `set_live_mode` → `add_track` → init segments → PTS remap →
    /// `on_session_started` → `notify_playing`) is performed inside `run()`
    /// before the fetch loop begins.
    /// Run the DASH fetch loop and emit [`PlayerEvent`] callbacks registered
    /// with [`Self::on`].
    pub async fn run(&mut self) -> Result<(), DashError> {
        if self.event_handlers.borrow().is_empty() {
            return self.session.run(&mut self.buf, &mut self.driver).await;
        }

        let handlers = Rc::clone(&self.event_handlers);
        emit_log(
            &handlers,
            PlayerLogLevel::Info,
            "player",
            "starting playback",
        );
        let mut buf = EventMediaBuffer::new(&mut self.buf, Rc::clone(&handlers));
        let stopped_by_driver = Rc::new(Cell::new(false));
        let mut driver = EventPlaybackDriver::new(
            &mut self.driver,
            Rc::clone(&handlers),
            Rc::clone(&stopped_by_driver),
        );
        let result = self.session.run(&mut buf, &mut driver).await;
        match &result {
            Ok(()) => {
                let reason = if stopped_by_driver.get() {
                    PlayerRunEndReason::StoppedByDriver
                } else {
                    PlayerRunEndReason::EndOfStream
                };
                emit_event(&handlers, PlayerEvent::RunEnded { reason });
                if reason == PlayerRunEndReason::EndOfStream {
                    emit_event(&handlers, PlayerEvent::EndOfStream);
                }
                emit_log(
                    &handlers,
                    PlayerLogLevel::Info,
                    "player",
                    format!("playback complete: {reason:?}"),
                );
            }
            Err(e) => {
                emit_log(
                    &handlers,
                    PlayerLogLevel::Error,
                    "player",
                    format!("playback failed: {e}"),
                );
            }
        }
        result
    }

    /// Register a callback for all [`PlayerEvent`] values emitted during
    /// [`run`][Self::run].
    pub fn on(&mut self, handler: impl FnMut(&PlayerEvent) + 'static) -> &mut Self {
        self.event_handlers.borrow_mut().push(Box::new(handler));
        self
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

// ── Event Wrappers ────────────────────────────────────────────────────────────

struct EventMediaBuffer<'a, S> {
    inner: &'a mut S,
    handlers: EventHandlers,
}

impl<'a, S> EventMediaBuffer<'a, S> {
    fn new(inner: &'a mut S, handlers: EventHandlers) -> Self {
        Self { inner, handlers }
    }
}

#[async_trait(?Send)]
impl<S: MediaBuffer> MediaBuffer for EventMediaBuffer<'_, S> {
    async fn open(&mut self) -> Result<(), DashError> {
        self.inner.open().await
    }

    fn set_duration(&mut self, secs: f64) {
        self.inner.set_duration(secs);
    }

    fn set_live_mode(&mut self, live: bool) {
        self.inner.set_live_mode(live);
    }

    async fn end_of_stream(&mut self) -> Result<(), DashError> {
        self.inner.end_of_stream().await
    }

    fn add_track(&mut self, mt: MediaType, mime_type: &str) -> bool {
        self.inner.add_track(mt, mime_type)
    }

    async fn change_type(&mut self, mt: MediaType, mime_type: &str) -> Result<(), DashError> {
        self.inner.change_type(mt, mime_type).await
    }

    fn set_timestamp_offset(&mut self, mt: MediaType, offset_s: f64) {
        self.inner.set_timestamp_offset(mt, offset_s);
    }

    fn set_append_window(&mut self, mt: MediaType, start: f64, end: f64) {
        self.inner.set_append_window(mt, start, end);
    }

    async fn append(&mut self, mt: MediaType, data: Bytes, is_init: bool) -> Result<(), DashError> {
        if !is_init {
            for emsg in parse_all_emsg(&data) {
                emit_log(
                    &self.handlers,
                    PlayerLogLevel::Info,
                    "player::emsg",
                    format!("emsg {} id {}", emsg.scheme_id_uri, emsg.id),
                );
                emit_event(
                    &self.handlers,
                    PlayerEvent::InbandDash(DashEvent {
                        media_type: mt,
                        scheme_id_uri: emsg.scheme_id_uri,
                        value: emsg.value,
                        presentation_time_s: emsg.presentation_time_s,
                        duration_s: emsg.duration_s,
                        id: emsg.id,
                        message_data: emsg.message_data,
                    }),
                );
            }
        }
        self.inner.append(mt, data, is_init).await
    }

    async fn remove_range(&mut self, mt: MediaType, start: f64, end: f64) -> Result<(), DashError> {
        self.inner.remove_range(mt, start, end).await
    }

    fn buffered(&self, mt: MediaType) -> Vec<TimeRange> {
        self.inner.buffered(mt)
    }

    fn has_buffer_tracking(&self) -> bool {
        self.inner.has_buffer_tracking()
    }
}

struct EventPlaybackDriver<'a, D> {
    inner: &'a mut D,
    handlers: EventHandlers,
    last_playback_state: Cell<PlaybackState>,
    stopped_by_driver: Rc<Cell<bool>>,
}

impl<'a, D: PlaybackDriver> EventPlaybackDriver<'a, D> {
    fn new(inner: &'a mut D, handlers: EventHandlers, stopped_by_driver: Rc<Cell<bool>>) -> Self {
        let last_playback_state = Cell::new(inner.playback_state());
        Self {
            inner,
            handlers,
            last_playback_state,
            stopped_by_driver,
        }
    }
}

#[async_trait(?Send)]
impl<D: PlaybackDriver> PlaybackDriver for EventPlaybackDriver<'_, D> {
    fn position_s(&self) -> f64 {
        self.inner.position_s()
    }

    fn playback_state(&self) -> PlaybackState {
        let state = self.inner.playback_state();
        let previous = self.last_playback_state.get();
        if previous != state {
            self.last_playback_state.set(state);
            emit_log(
                &self.handlers,
                PlayerLogLevel::Info,
                "player::playback_state",
                format!("playback state {previous:?} -> {state:?}"),
            );
            emit_event(
                &self.handlers,
                PlayerEvent::PlaybackStateChanged {
                    from: previous,
                    to: state,
                },
            );
        }
        state
    }

    async fn on_gap_seek(&mut self, target: GapSeekTarget) {
        self.inner.on_gap_seek(target.clone()).await;
        emit_log(
            &self.handlers,
            PlayerLogLevel::Warn,
            "player::gap",
            format!("gap seek -> {:.2}s", target.position_s),
        );
        emit_event(&self.handlers, PlayerEvent::GapSeek(target));
    }

    fn throughput_override_bps(&self) -> Option<f64> {
        self.inner.throughput_override_bps()
    }

    fn audio_gate_target_ms(&self, video_buffer_ms: f64, default_ms: f64) -> f64 {
        self.inner.audio_gate_target_ms(video_buffer_ms, default_ms)
    }

    fn enable_pruning(&self) -> bool {
        self.inner.enable_pruning()
    }

    fn should_continue(&self) -> bool {
        let should_continue = self.inner.should_continue();
        if !should_continue {
            self.stopped_by_driver.set(true);
        }
        should_continue
    }

    fn pending_seek_s(&mut self) -> Option<f64> {
        self.inner.pending_seek_s()
    }

    fn has_pending_seek(&self) -> bool {
        self.inner.has_pending_seek()
    }

    fn on_iteration_start(
        &mut self,
        session_position_s: f64,
        video_buffer_level_ms: f64,
        audio_buffer_level_ms: f64,
    ) {
        self.inner.on_iteration_start(
            session_position_s,
            video_buffer_level_ms,
            audio_buffer_level_ms,
        );
    }

    fn on_quality_change(
        &mut self,
        media_type: MediaType,
        old_index: usize,
        new_index: usize,
        reason: AbrDecisionReason,
    ) {
        self.inner
            .on_quality_change(media_type, old_index, new_index, reason.clone());
        emit_log(
            &self.handlers,
            PlayerLogLevel::Info,
            "player::abr",
            format!("{media_type:?} quality {old_index} -> {new_index} ({reason})"),
        );
        emit_event(
            &self.handlers,
            PlayerEvent::QualityChange {
                media_type,
                old_index,
                new_index,
                reason,
            },
        );
    }

    fn on_iteration(&mut self, summary: &crate::session::driver::IterationSummary<'_>) {
        self.inner.on_iteration(summary);
        emit_event(
            &self.handlers,
            PlayerEvent::Iteration(PlayerIterationEvent {
                position_s: summary.position_s,
                video_index: summary.video_decision.map(|d| d.index),
                audio_index: summary.audio_decision.map(|d| d.index),
                video_bandwidth: summary.video_bandwidth,
                audio_bandwidth: summary.audio_bandwidth,
                video_buffer_ms: summary.video_buffer_ms,
                audio_buffer_ms: summary.audio_buffer_ms,
                buffer_target_ms: summary.buffer_target_ms,
                http_request_count: summary.http_request_count,
                switch_count: summary.switch_count,
                last_transfer_url: summary.last_transfer_url.clone(),
                last_transfer_ms: summary.last_transfer_ms,
                last_throughput_bps: summary.last_throughput_bps,
                video_rep_count: summary.video_rep_count,
                audio_rep_count: summary.audio_rep_count,
                is_live: summary.is_live,
            }),
        );
    }

    fn wants_pts_remap(&self) -> bool {
        self.inner.wants_pts_remap()
    }

    async fn on_session_started(&mut self, start_s: f64) {
        self.inner.on_session_started(start_s).await;
        emit_log(
            &self.handlers,
            PlayerLogLevel::Info,
            "player",
            format!("session started @ {start_s:.2}s"),
        );
        emit_event(&self.handlers, PlayerEvent::SessionStarted { start_s });
    }
}
