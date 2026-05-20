//! [`PlaybackDriver`] trait and [`IterationSummary`] — the caller-side
//! interface for [`super::DashSession::run`].
//!
//! # Responsibilities split
//!
//! | What lives in the library (`run`) | What lives in the driver |
//! |---|---|
//! | `video_done` / `audio_done` tracking | Renderer playback position |
//! | Buffer gate (video + audio) | Renderer pause state |
//! | Gap-controller tick | Renderer seek (on gap fill) |
//! | `fetch_next_segment` + EOS detection | External throughput measurement |
//! | `end_of_stream` teardown | Custom audio gate threshold |
//! | `prune_all_tracks` (when enabled) | Per-iteration progress reporting |

use async_trait::async_trait;

#[cfg(not(feature = "std"))]
use alloc::{boxed::Box, string::String};

use crate::{
    abr::{AbrDecisionReason, QualityDecision},
    session::{GapSeekTarget, MediaType},
};

// ── PlaybackState ─────────────────────────────────────────────────────────────

/// Current renderer playback state, reported to the session each iteration via
/// [`PlaybackDriver::playback_state`].
///
/// The session uses this to decide:
/// - Whether to **freeze the wall-clock** used by
///   [`DashSession::estimated_buffer_level_ms`] — both `Paused` and `Buffering`
///   freeze it, so the estimated buffer level does not appear to drain while the
///   renderer is not consuming data.
/// - Whether to **suppress gap-jump seeks** issued by the
///   [`GapController`][super::GapController] — suppressed for `Paused` and
///   `Buffering`, since jumping away from a stall would discard data already
///   being fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackState {
    /// The renderer is actively consuming data.  The session's wall-clock runs.
    #[default]
    Playing,
    /// The renderer has been paused by the user.  The wall-clock is frozen and
    /// gap-jump seeks are suppressed.
    Paused,
    /// The renderer is stalled waiting for data (not a user action).  Treated
    /// identically to `Paused` for clock-freezing and gap-jump purposes, but
    /// distinguished so that callers can log or react differently.
    Buffering,
}

impl PlaybackState {
    /// Returns `true` when the renderer is not actively consuming data and the
    /// session wall-clock should be frozen.
    pub fn freezes_clock(self) -> bool {
        !matches!(self, Self::Playing)
    }

    /// Returns `true` when gap-jump seeks should be suppressed.
    pub fn suppresses_gap_jump(self) -> bool {
        !matches!(self, Self::Playing)
    }

    /// Returns `true` when the renderer is playing (neither paused nor stalled).
    pub fn is_playing(self) -> bool {
        matches!(self, Self::Playing)
    }
}

// ── IterationSummary ──────────────────────────────────────────────────────────

/// Per-iteration snapshot passed to [`PlaybackDriver::on_iteration`].
///
/// All fields are derived from session state immediately after both segments
/// have been fetched.  Use these values for progress reporting, UI updates,
/// or status panel pushes without needing a direct reference to the session.
#[derive(Debug)]
pub struct IterationSummary<'a> {
    /// Session estimated playback position at the start of this iteration (s).
    pub position_s: f64,

    /// Quality decision for the video segment fetched this iteration, or
    /// `None` if the video track is absent or already at end-of-stream.
    pub video_decision: Option<&'a QualityDecision>,

    /// Quality decision for the audio segment fetched this iteration, or
    /// `None` if the audio track is absent or already at end-of-stream.
    pub audio_decision: Option<&'a QualityDecision>,

    /// Declared bandwidth (bps) of the video representation that was active
    /// during this iteration, or `0` if the video track is absent.
    pub video_bandwidth: u64,

    /// Declared bandwidth (bps) of the audio representation that was active
    /// during this iteration, or `0` if the audio track is absent.
    pub audio_bandwidth: u64,

    /// Estimated video buffer level at the end of this iteration (ms).
    pub video_buffer_ms: f64,

    /// Estimated audio buffer level at the end of this iteration (ms).
    pub audio_buffer_ms: f64,

    /// Session buffer target (ms) — the fill goal the gate waits for.
    pub buffer_target_ms: f64,

    /// Total HTTP request records accumulated in [`crate::DashMetrics`].
    pub http_request_count: usize,

    /// Total representation-switch records accumulated in [`crate::DashMetrics`].
    pub switch_count: usize,

    /// URL of the most recently completed HTTP request, or an empty string.
    pub last_transfer_url: String,

    /// Transfer duration of the most recently completed HTTP request (ms).
    pub last_transfer_ms: f64,

    /// Measured throughput of the most recently completed HTTP request (bps),
    /// or `None` if no request has completed yet.
    pub last_throughput_bps: Option<f64>,

    /// Total number of video representations available in the current period,
    /// or `0` if the video track is absent.
    pub video_rep_count: u32,

    /// Total number of audio representations available in the current period,
    /// or `0` if the audio track is absent.
    pub audio_rep_count: u32,

    /// Whether the session is a live stream (`MPD@type="dynamic"`).
    pub is_live: bool,
}

// ── PlaybackDriver ────────────────────────────────────────────────────────────

/// Caller-side interface for [`super::DashSession::run`].
///
/// Implement this trait to plug your renderer — a GStreamer pipeline, a
/// browser `<video>` element, a test harness, or a headless benchmark — into
/// the library's fetch loop.
///
/// Every method except [`position_s`][Self::position_s] has a sensible default
/// so you only need to override what your platform actually requires.
///
/// # Required annotation: `#[async_trait(?Send)]`
///
/// This trait uses `async-trait` and **must** be implemented with the
/// `?Send` variant:
///
/// ```rust,ignore
/// use async_trait::async_trait;
/// use dash_rs::PlaybackDriver;
///
/// struct MyDriver;
///
/// #[async_trait(?Send)]   // ← the ?Send is required
/// impl PlaybackDriver for MyDriver {
///     fn position_s(&self) -> f64 { 0.0 }
/// }
/// ```
///
/// Using `#[async_trait]` (without `?Send`) produces a cryptic
/// "associated type not found" compile error.  The `?Send` bound is
/// required because `dash-rs` targets `wasm32-unknown-unknown` where most
/// types are not `Send`.
///
/// # Minimal implementation
///
/// ```rust,ignore
/// use async_trait::async_trait;
/// use dash_rs::PlaybackDriver;
///
/// struct MyDriver;
///
/// #[async_trait(?Send)]
/// impl PlaybackDriver for MyDriver {
///     fn position_s(&self) -> f64 { 0.0 }
/// }
/// ```
#[async_trait(?Send)]
pub trait PlaybackDriver {
    // ── Required ─────────────────────────────────────────────────────────────

    /// Current renderer playback position in seconds.
    ///
    /// Used for the gap-controller tick and for building [`crate::AbrMetrics`].
    /// When no external position is available (headless renderer or test
    /// harness), return `0.0`.  [`super::DashSession::estimated_playback_position_s`]
    /// is a reasonable alternative — cache it in [`on_iteration_start`] and
    /// return the cached value here.
    ///
    /// [`on_iteration_start`]: Self::on_iteration_start
    fn position_s(&self) -> f64;

    // ── Optional ─────────────────────────────────────────────────────────────

    /// Current renderer playback state.
    ///
    /// Called every loop iteration so that the session's wall-clock and
    /// gap-controller behave correctly while the renderer is paused or stalled.
    ///
    /// - [`PlaybackState::Paused`] and [`PlaybackState::Buffering`] both freeze
    ///   [`DashSession::estimated_buffer_level_ms`] and suppress gap-jump seeks.
    /// - [`PlaybackState::Playing`] (the default) lets the clock run freely.
    ///
    /// Default: [`PlaybackState::Playing`] — always playing.
    fn playback_state(&self) -> PlaybackState {
        PlaybackState::Playing
    }

    /// Called when the gap controller recommends a seek.
    ///
    /// By the time this is called the session's segment-fetch position and
    /// gap-controller state have already been reset via
    /// [`DashSession::seek_to`][super::DashSession::seek_to].
    /// Implementors should seek the renderer to `target.position_s`.
    ///
    /// Default: no-op.  Suitable for GStreamer which uses the GLib bus for
    /// stall detection rather than the soft gap controller.
    async fn on_gap_seek(&mut self, _target: GapSeekTarget) {}

    /// Optional external throughput measurement in bits per second.
    ///
    /// When `Some(bps)` is returned, `run()` overwrites
    /// `AbrMetrics::throughput_bps` before passing metrics to
    /// `fetch_next_segment`.  This lets a fetcher that measures real transfer
    /// times feed accurate bandwidth data to the ABR controller.
    ///
    /// Default: `None` — the library's internal `ThroughputTracker` is used.
    fn throughput_override_bps(&self) -> Option<f64> {
        None
    }

    /// Override the buffer gate threshold for the **audio** track (ms).
    ///
    /// `run()` calls this with the current video buffer level (after the video
    /// fetch) and the session's default `buffer_target_ms`, then uses the
    /// return value as the gate threshold before fetching the next audio
    /// segment.  Return `default_ms` to use the same threshold for both tracks
    /// (the default).
    ///
    /// The GStreamer player overrides this to slave audio buffering to the
    /// video level, preventing audio from racing ahead of video:
    ///
    /// ```rust,ignore
    /// fn audio_gate_target_ms(&self, video_buffer_ms: f64, default_ms: f64) -> f64 {
    ///     video_buffer_ms.min(default_ms) + 1_000.0
    /// }
    /// ```
    fn audio_gate_target_ms(&self, _video_buffer_ms: f64, default_ms: f64) -> f64 {
        default_ms
    }

    /// Whether `run()` should call
    /// [`prune_all_tracks`][super::DashSession::prune_all_tracks] at the start
    /// of each iteration.
    ///
    /// Default: `false`.  Enable for native renderers (e.g. GStreamer) where
    /// the MSE append window does not automatically evict old buffer data and
    /// memory must be managed explicitly.
    fn enable_pruning(&self) -> bool {
        false
    }

    /// Return `false` to exit the fetch loop before end-of-stream.
    ///
    /// `run()` checks this at the top of every iteration (after the
    /// `video_done && audio_done` end-of-stream check).  Returning `false`
    /// exits the loop and calls `end_of_stream()`.
    ///
    /// Use this to implement bounded examples:
    ///
    /// ```rust,ignore
    /// fn should_continue(&self) -> bool {
    ///     self.segments_fetched < MAX_SEGMENTS
    /// }
    /// ```
    ///
    /// Default: `true` — run until all tracks signal end-of-stream.
    fn should_continue(&self) -> bool {
        true
    }

    /// Returns a pending user-initiated seek position in seconds, if any.
    ///
    /// Called at the start of each `run()` iteration, immediately after
    /// [`should_continue`][Self::should_continue].  When `Some(pos)` is
    /// returned the session:
    ///
    /// 1. Repositions the segment-fetch cursor via [`DashSession::seek_to`].
    /// 2. Resets the wall-clock baseline via [`DashSession::notify_playing`]
    ///    so that [`DashSession::estimated_buffer_level_ms`] counts from zero
    ///    at the new position.
    /// 3. Clears any end-of-stream flags so tracks that finished before the
    ///    seek target resume fetching.
    ///
    /// The implementation **must** clear the pending seek before returning so
    /// that subsequent calls return `None` (destructive read).  Rapid seeks
    /// coalesce naturally — only the most recent position matters.
    ///
    /// Default: `None` — no seek pending.
    fn pending_seek_s(&mut self) -> Option<f64> {
        None
    }

    /// Returns `true` when a user-initiated seek is pending **without**
    /// consuming it.
    ///
    /// This is a non-destructive companion to
    /// [`pending_seek_s`][Self::pending_seek_s].  It is queried by the
    /// buffer-gate loops inside [`DashSession::run`] to bail out early when a
    /// new seek arrives while the gate is sleeping.  Without this, a seek that
    /// fires while the video gate is blocked waits up to one full
    /// `buffer_target_ms` worth of 500 ms gate-loop ticks before being
    /// processed, leaving the browser stalled with no data at the new seek
    /// target.
    ///
    /// **Override this method whenever you override `pending_seek_s`.**
    /// The default returns `false` (never any seek pending).
    fn has_pending_seek(&self) -> bool {
        false
    }

    /// Called at the **start** of each loop iteration, before the buffer gate
    /// and fetch.  Receives the session's current estimated playback position
    /// and per-track buffer levels (in milliseconds).
    ///
    /// Use this to:
    /// - Advance a per-iteration counter or state machine before
    ///   `throughput_override_bps` or `should_continue` is queried.
    /// - Cache the session position so that `position_s()` can return it
    ///   without holding a reference to the session (GStreamer pattern).
    /// - Forward buffer levels to `MediaBuffer::buffered()` so that the ABR
    ///   controller receives a non-zero buffer level and can select higher
    ///   quality representations.
    ///
    /// Default: no-op.
    fn on_iteration_start(
        &mut self,
        _session_position_s: f64,
        _video_buffer_level_ms: f64,
        _audio_buffer_level_ms: f64,
    ) {}

    /// Called immediately when the ABR controller switches to a different
    /// quality level during a fetch.
    ///
    /// This fires **before** [`on_iteration`][Self::on_iteration] in the same
    /// loop iteration so that drivers can react to the switch (e.g. update a
    /// bitrate indicator) as soon as it happens.
    ///
    /// Only fires on actual switches — if the ABR controller selects the same
    /// index as the previous fetch, this method is not called.  It also does
    /// not fire for the **first** segment fetch (there is no previous index to
    /// switch from).
    ///
    /// # Arguments
    ///
    /// - `media_type` — which track switched (`Video` or `Audio`).
    /// - `old_index` — representation index that was active before the switch.
    /// - `new_index` — representation index just selected.
    /// - `reason` — why the ABR controller chose the new index.
    ///
    /// Default: no-op.
    fn on_quality_change(
        &mut self,
        _media_type: MediaType,
        _old_index: usize,
        _new_index: usize,
        _reason: AbrDecisionReason,
    ) {
    }

    /// Called after **both** tracks have been fetched (or skipped at EOS).
    ///
    /// Use for progress reporting, UI updates, and status panel pushes.
    ///
    /// Default: no-op.
    fn on_iteration(&mut self, _summary: &IterationSummary<'_>) {}

    /// Whether `run()` should apply live-stream PTS remapping before starting
    /// the fetch loop.
    ///
    /// When `true`, `run()` calls
    /// [`DashSession::apply_live_pts_remap`][super::DashSession::apply_live_pts_remap]
    /// on live streams so that epoch-scale fMP4 timestamps are shifted to ≈ 0.
    /// This is the correct behaviour for browser MSE players (WASM).
    ///
    /// **GStreamer clients must return `false`** (the default) because GStreamer
    /// applies its own probe-based PTS subtraction via `set_live_mode(true)`.
    /// Double-applying would corrupt timestamps.
    ///
    /// Default: `false`.
    fn wants_pts_remap(&self) -> bool {
        false
    }

    /// Called by `run()` after the startup sequence completes and immediately
    /// before the fetch loop begins.
    ///
    /// `start_s` is the presentation-timeline position at which playback will
    /// start:
    /// - VOD: `0.0`.
    /// - Live with PTS remap (`wants_pts_remap() == true`): ≈ `0.0` (the
    ///   remapped live-edge presentation time).
    /// - Live without remap (`wants_pts_remap() == false`): the raw
    ///   `live_presentation_start_s()` value (may be epoch-scale for CMAF
    ///   streams).
    ///
    /// Use this to:
    /// - Seek the renderer to `start_s` before playback begins (WASM pattern).
    /// - Transition the pipeline to Playing after init segments have been
    ///   appended (GStreamer pattern).
    ///
    /// Default: no-op.
    async fn on_session_started(&mut self, _start_s: f64) {}
}
