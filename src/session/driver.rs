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

use crate::{abr::QualityDecision, session::GapSeekTarget};

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
    /// or `0.0` if no request has completed yet.
    pub last_throughput_bps: f64,
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
/// # Minimal implementation
///
/// ```rust,ignore
/// use async_trait::async_trait;
/// use dash_rs::session::driver::{IterationSummary, PlaybackDriver};
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

    /// Whether the renderer is currently paused.
    ///
    /// Called every loop iteration so that the session's buffer-level estimate
    /// stays accurate while the renderer is paused.
    ///
    /// Default: `false` (always playing).
    fn is_paused(&self) -> bool {
        false
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

    /// Called at the **start** of each loop iteration, before the buffer gate
    /// and fetch.  Receives the session's current estimated playback position.
    ///
    /// Use this to:
    /// - Advance a per-iteration counter or state machine before
    ///   `throughput_override_bps` or `should_continue` is queried.
    /// - Cache the session position so that `position_s()` can return it
    ///   without holding a reference to the session (GStreamer pattern).
    ///
    /// Default: no-op.
    fn on_iteration_start(&mut self, _session_position_s: f64) {}

    /// Called after **both** tracks have been fetched (or skipped at EOS).
    ///
    /// Use for progress reporting, UI updates, and status panel pushes.
    ///
    /// Default: no-op.
    fn on_iteration(&mut self, _summary: &IterationSummary<'_>) {}
}
