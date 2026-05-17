//! [`GstPlaybackDriver`] — [`dash_rs::PlaybackDriver`] implementation for the
//! GStreamer player.
//!
//! - Uses the session's own estimated position (no external renderer query).
//! - Slaves the audio gate to the video buffer level + 1 s so that audio and
//!   video stay in lock-step (mirrors dash.js `ScheduleController`).
//! - Enables buffer pruning so old data is evicted from GStreamer's `appsrc`.
//! - Transitions the pipeline to `Playing` in `on_session_started` — after
//!   init segments have been appended and `set_live_mode` has been called.
//! - Pushes a console progress row after every segment pair.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use async_trait::async_trait;
use dash_rs::session::driver::{IterationSummary, PlaybackDriver, PlaybackState};
use gst::prelude::*;
use gstreamer as gst;

// ── GstPlaybackDriver ─────────────────────────────────────────────────────────

pub(crate) struct GstPlaybackDriver {
    pub(crate) is_paused: Arc<AtomicBool>,
    pub(crate) is_buffering: Arc<AtomicBool>,
    pub(crate) throughput_tracker: Arc<AtomicU64>,
    /// Cloned pipeline handle — used to call `set_state(Playing)` in
    /// `on_session_started` after init segments have been pushed.
    pipeline: gst::Pipeline,
    /// Session's estimated playback position — updated at iteration start.
    cached_pos_s: f64,
    seg: u32,
}

impl GstPlaybackDriver {
    pub(crate) fn new(
        is_paused: Arc<AtomicBool>,
        is_buffering: Arc<AtomicBool>,
        throughput_tracker: Arc<AtomicU64>,
        pipeline: gst::Pipeline,
    ) -> Self {
        Self {
            is_paused,
            is_buffering,
            throughput_tracker,
            pipeline,
            cached_pos_s: 0.0,
            seg: 0,
        }
    }
}

#[async_trait(?Send)]
impl PlaybackDriver for GstPlaybackDriver {
    fn position_s(&self) -> f64 {
        self.cached_pos_s
    }

    fn playback_state(&self) -> PlaybackState {
        // Buffering takes priority — a stalled renderer is not user-paused.
        if self.is_buffering.load(Ordering::Relaxed) {
            PlaybackState::Buffering
        } else if self.is_paused.load(Ordering::Relaxed) {
            PlaybackState::Paused
        } else {
            PlaybackState::Playing
        }
    }

    fn throughput_override_bps(&self) -> Option<f64> {
        Some(f64::from_bits(
            self.throughput_tracker.load(Ordering::Relaxed),
        ))
    }

    fn audio_gate_target_ms(&self, video_buffer_ms: f64, default_ms: f64) -> f64 {
        // Slave audio target to video level so audio never races ahead.
        // For audio-only streams video_buffer_ms is 0, so we fall back
        // to the session default.
        if video_buffer_ms > 0.0 {
            video_buffer_ms.min(default_ms) + 1_000.0
        } else {
            default_ms
        }
    }

    fn enable_pruning(&self) -> bool {
        true
    }

    fn on_iteration_start(&mut self, session_position_s: f64) {
        self.cached_pos_s = session_position_s;
        self.seg += 1;
    }

    /// Transition the GStreamer pipeline to `Playing`.
    ///
    /// Called by `run()` after `set_live_mode` and init segments have been
    /// applied, so the `pad-added` callback fires with the correct live-mode
    /// flag and the decoder has codec parameters from the init segment.
    async fn on_session_started(&mut self, _start_s: f64) {
        match self.pipeline.set_state(gst::State::Playing) {
            Ok(_) => tracing::info!("GstPlaybackDriver: pipeline set to Playing"),
            Err(e) => tracing::error!("GstPlaybackDriver: set_state(Playing) failed: {e}"),
        }
    }

    fn on_iteration(&mut self, s: &IterationSummary<'_>) {
        let measured_bps = f64::from_bits(self.throughput_tracker.load(Ordering::Relaxed));
        let buf_ms = if s.video_buffer_ms > 0.0 {
            s.video_buffer_ms
        } else {
            s.audio_buffer_ms
        };

        let (vid_idx, vid_reason, video_ms) = s
            .video_decision
            .map(|d| {
                (
                    d.index.to_string(),
                    format!("{:?}", d.reason),
                    s.last_transfer_ms as u128,
                )
            })
            .unwrap_or_else(|| ("done".into(), "-".into(), 0));
        let (aud_idx, aud_reason) = s
            .audio_decision
            .map(|d| (d.index.to_string(), format!("{:?}", d.reason)))
            .unwrap_or_else(|| ("done".into(), "-".into()));

        println!(
            "  {:<4}  {:<8}  {:<12}  {:<8}  {:<12}  {:>7.1} s   {:>4} ms  ({:.0} kbps)",
            self.seg,
            vid_idx,
            vid_reason,
            aud_idx,
            aud_reason,
            buf_ms / 1_000.0,
            video_ms,
            measured_bps / 1_000.0,
        );
    }
}
