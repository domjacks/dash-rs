//! Gap controller — stall detection and proactive gap-jump logic.
//!
//! Mirrors [`GapController.js`](https://github.com/Dash-Industry-Forum/dash.js/blob/development/src/streaming/controllers/GapController.js)
//! in dash.js.
//!
//! # Responsibility
//!
//! The `GapController` is a pure state machine: it accepts a playback
//! position and the current set of buffered time-ranges on each call to
//! [`GapController::tick`], and returns a [`GapSeekTarget`] whenever the
//! player should seek forward to escape a gap or a stall.  It has no I/O
//! and performs no network requests.
//!
//! # Caller contract
//!
//! 1. Call [`GapController::tick`] periodically (e.g. every 200 ms — the
//!    dash.js `streaming.gaps.checkInterval` default) with the renderer's
//!    current playback position and the union of buffered ranges across all
//!    tracks.
//! 2. If `tick` returns `Some(target)`, seek the renderer to `target.position_s`
//!    **and** call [`DashSession::seek_to`] so the segment fetcher is
//!    repositioned to the new time.
//! 3. After the seek completes, call [`GapController::on_seek`] to clear
//!    state so the same position is not jumped a second time.
//! 4. Call [`GapController::on_track_switch_started`] /
//!    [`GapController::on_track_switch_ended`] around buffer-replacement
//!    operations (quality switches) to suppress gap jumps during that window.
//!
//! # Gap-detection algorithm
//!
//! Mirrors `_getNextRangeIndex` in `GapController.js`:
//!
//! 1. Walk the buffered ranges in order.
//! 2. Find the first range `R` whose start is ahead of `position_s`.
//! 3. The gap is jumpable when the distance from the end of the *previous*
//!    range to `position_s` is less than [`GapControllerSettings::threshold_s`]
//!    (i.e. the player is at or past the edge of buffered content).
//! 4. Small gaps (`gap ≤ small_gap_limit_s`) are always jumped; large gaps
//!    are jumped only when [`GapControllerSettings::jump_large_gaps`] is `true`.
//!
//! # Stall detection
//!
//! If the playback position has not changed for
//! [`GapControllerSettings::stall_threshold_ticks`] consecutive `tick` calls
//! (default 10, matching `THRESHOLD_TO_STALLS` in dash.js), the controller
//! tries an additional stall-recovery seek:
//!
//! - If a gap is visible: jump it (same logic as above).
//! - If position is inside a buffered range but the player has stalled
//!   (decoder hiccup / micro-gap): seek forward by
//!   [`GapControllerSettings::stall_seek_s`] (default 0.1 s).
//!
//! # Not yet implemented
//!
//! - **Period-end stall jump**: when the player stalls near the end of a
//!   period and `timeToStreamEnd < smallGapLimit`, dash.js seeks to the
//!   exact stream end.  Requires knowing the period's end time.
//!   Call `DashSession::seek_to(period_end_s)` manually in the application
//!   if needed.

use crate::media_buffer::TimeRange;

// ── Settings ──────────────────────────────────────────────────────────────────

/// Tuning parameters for [`GapController`].
///
/// All defaults match the corresponding `settings.streaming.gaps.*` values in
/// dash.js.
#[derive(Debug, Clone)]
pub struct GapControllerSettings {
    /// Maximum gap size (seconds) that is always jumped, regardless of
    /// [`Self::jump_large_gaps`].
    ///
    /// dash.js `streaming.gaps.smallGapLimit` — default **0.8 s**.
    pub small_gap_limit_s: f64,

    /// Whether to also jump gaps larger than [`Self::small_gap_limit_s`].
    ///
    /// dash.js `streaming.gaps.jumpLargeGaps` — default **`true`**.
    pub jump_large_gaps: bool,

    /// Offset added to the gap-start position when building the seek target.
    ///
    /// Lands the renderer slightly inside the next buffered range rather than
    /// exactly on its boundary, which avoids re-triggering the gap detector on
    /// the very next tick.
    ///
    /// dash.js `streaming.gaps.seekOffset` — default **0.1 s**.
    pub seek_offset_s: f64,

    /// How far ahead to seek when stalled inside a buffered range with no
    /// visible gap (decoder hiccup / micro-gap recovery).
    ///
    /// dash.js `streaming.gaps.stallSeek` — default **0.1 s**.
    pub stall_seek_s: f64,

    /// Number of consecutive [`GapController::tick`] calls with an unchanged
    /// position before a stall is declared.
    ///
    /// dash.js `THRESHOLD_TO_STALLS` constant — default **10**.
    pub stall_threshold_ticks: u32,

    /// Floating-point proximity threshold used in gap detection.
    ///
    /// A gap is only detected when the distance from the end of the previous
    /// buffered range to `position_s` is less than this value.  Ranges
    /// separated by less than this are treated as continuous.
    ///
    /// dash.js `streaming.gaps.threshold` — default **0.1 s**.
    pub threshold_s: f64,
}

impl Default for GapControllerSettings {
    fn default() -> Self {
        Self {
            small_gap_limit_s: 0.8,
            jump_large_gaps: true,
            seek_offset_s: 0.1,
            stall_seek_s: 0.1,
            stall_threshold_ticks: 10,
            threshold_s: 0.1,
        }
    }
}

// ── Seek target ───────────────────────────────────────────────────────────────

/// Reason for the seek recommended by [`GapController::tick`].
#[derive(Debug, Clone, PartialEq)]
pub enum GapSeekReason {
    /// A decoded-buffer gap was detected between `gap_start_s` and
    /// `gap_end_s`.  The target position is `gap_end_s +
    /// seek_offset_s`.
    GapJump {
        /// Start of the gap (end of the previous buffered range, or 0.0 if
        /// the gap is at the very beginning).
        gap_start_s: f64,
        /// End of the gap (start of the next buffered range).
        gap_end_s: f64,
    },
    /// The player was stalled inside a buffered range with no visible gap.
    /// This indicates a decoder hiccup or micro-gap below the detection
    /// threshold.
    StallRecovery,
}

/// A seek recommendation returned by [`GapController::tick`].
#[derive(Debug, Clone, PartialEq)]
pub struct GapSeekTarget {
    /// Position (in seconds) to seek the renderer to.
    pub position_s: f64,
    /// Why the controller is recommending this seek.
    pub reason: GapSeekReason,
}

// ── GapController ─────────────────────────────────────────────────────────────

/// Stall-detection and gap-jump state machine.
///
/// See the [module-level documentation](self) for the caller contract and
/// algorithm description.
///
/// # Example
///
/// ```
/// use dash_rs::session::{GapController, GapSeekTarget};
/// use dash_rs::media_buffer::TimeRange;
///
/// let mut gap_ctrl = GapController::new();
///
/// // Buffered: [0.0, 3.0] then gap, then [4.0, 8.0].
/// let buffered = vec![
///     TimeRange::new(0.0, 3.0),
///     TimeRange::new(4.0, 8.0),
/// ];
///
/// // Player is at 3.05 s — just past the end of the first range, in a gap.
/// // After enough stall ticks the gap controller will recommend a seek.
/// let target = gap_ctrl.tick(3.05, &buffered, false);
/// assert!(target.is_some());
/// let t = target.unwrap();
/// // Seek target lands inside the next range (4.0 + seek_offset 0.1 = 4.1 s).
/// assert!((t.position_s - 4.1).abs() < 1e-9);
/// ```
#[derive(Debug, Clone)]
pub struct GapController {
    settings: GapControllerSettings,
    /// Position reported on the most recent `tick` call.
    last_position_s: Option<f64>,
    /// Position most recently jumped to, to prevent re-jumping the same gap.
    last_jump_position_s: Option<f64>,
    /// Consecutive ticks on which position has not changed.
    stall_ticks: u32,
    /// When `true`, gap jumps are suppressed (buffer replacement in progress).
    track_switch_in_progress: bool,
}

impl Default for GapController {
    fn default() -> Self {
        Self::new()
    }
}

impl GapController {
    /// Create a new controller with default settings.
    pub fn new() -> Self {
        Self::with_settings(GapControllerSettings::default())
    }

    /// Create a new controller with custom settings.
    pub fn with_settings(settings: GapControllerSettings) -> Self {
        Self {
            settings,
            last_position_s: None,
            last_jump_position_s: None,
            stall_ticks: 0,
            track_switch_in_progress: false,
        }
    }

    /// Return the current settings.
    pub fn settings(&self) -> &GapControllerSettings {
        &self.settings
    }

    /// Replace the current settings.
    pub fn set_settings(&mut self, settings: GapControllerSettings) {
        self.settings = settings;
    }

    /// Suppress gap jumps during a buffer-replacement operation (e.g. a
    /// quality switch that clears the source buffer).
    ///
    /// Call [`Self::on_track_switch_ended`] when the replacement is complete.
    pub fn on_track_switch_started(&mut self) {
        self.track_switch_in_progress = true;
    }

    /// Re-enable gap jumps after a buffer-replacement operation completes.
    pub fn on_track_switch_ended(&mut self) {
        self.track_switch_in_progress = false;
    }

    /// Reset internal state after the renderer has completed a seek.
    ///
    /// Clears stall counters and the last-jump-position guard so the
    /// controller does not mistake the post-seek position change for a new
    /// stall, and so the new position can be jumped if another gap is
    /// encountered there.
    pub fn on_seek(&mut self) {
        self.last_position_s = None;
        self.last_jump_position_s = None;
        self.stall_ticks = 0;
    }

    /// Reset all state to its initial values.
    pub fn reset(&mut self) {
        self.last_position_s = None;
        self.last_jump_position_s = None;
        self.stall_ticks = 0;
        self.track_switch_in_progress = false;
    }

    /// Examine the current playback state and return a seek recommendation if
    /// a gap or stall requires intervention.
    ///
    /// # Arguments
    ///
    /// * `position_s` — current renderer playback position in seconds.
    /// * `buffered` — buffered time ranges (e.g. union of `SourceBuffer::buffered()`
    ///   across all tracks), sorted ascending by `start`.
    /// * `is_paused` — `true` while the renderer is paused.  Gap jumps are
    ///   suppressed while paused, but stall counters are still updated.
    ///
    /// # Returns
    ///
    /// `Some(target)` when the renderer should seek to `target.position_s`;
    /// `None` when no action is required.
    pub fn tick(
        &mut self,
        position_s: f64,
        buffered: &[TimeRange],
        is_paused: bool,
    ) -> Option<GapSeekTarget> {
        // Suppress gap jumps during buffer replacement.
        if self.track_switch_in_progress {
            self.last_position_s = Some(position_s);
            return None;
        }

        // Update stall counter.
        let stalled = if let Some(last) = self.last_position_s {
            if (position_s - last).abs() < self.settings.threshold_s {
                self.stall_ticks = self.stall_ticks.saturating_add(1);
                self.stall_ticks >= self.settings.stall_threshold_ticks
            } else {
                // Position changed — reset stall counter and allow re-jumping.
                self.stall_ticks = 0;
                self.last_jump_position_s = None;
                false
            }
        } else {
            false
        };
        self.last_position_s = Some(position_s);

        // Suppress gap jumps (but not stall detection) while paused.
        if is_paused {
            return None;
        }

        // Find the first buffered range that starts ahead of position_s and is
        // reachable (player is at or near the end of the preceding range).
        let next = self.find_next_range(position_s, buffered);

        let mut seek_pos: Option<(f64, GapSeekReason)> = None;

        if let Some((gap_start_s, gap_end_s)) = next {
            let gap = gap_end_s - position_s;
            if gap > 0.0
                && (gap <= self.settings.small_gap_limit_s || self.settings.jump_large_gaps)
            {
                seek_pos = Some((
                    gap_end_s + self.settings.seek_offset_s,
                    GapSeekReason::GapJump {
                        gap_start_s,
                        gap_end_s,
                    },
                ));
            }
        }

        // Stall recovery when no gap is visible but player is stuck in buffer.
        if seek_pos.is_none() && stalled && Self::is_position_buffered(position_s, buffered) {
            seek_pos = Some((
                position_s + self.settings.stall_seek_s,
                GapSeekReason::StallRecovery,
            ));
        }

        // Deduplicate: do not emit the same seek target twice in a row.
        if let Some((pos, reason)) = seek_pos {
            if pos <= position_s {
                return None;
            }
            if self.last_jump_position_s == Some(pos) {
                return None;
            }
            tracing::warn!(
                position_s,
                target_s = pos,
                reason = ?reason,
                "GapController: recommending seek to escape gap/stall"
            );
            self.last_jump_position_s = Some(pos);
            self.stall_ticks = 0;
            Some(GapSeekTarget {
                position_s: pos,
                reason,
            })
        } else {
            None
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Find the first buffered range whose start is ahead of `position_s`,
    /// subject to the proximity threshold.
    ///
    /// Returns `(gap_start_s, gap_end_s)` where `gap_end_s` is the start of
    /// the next buffered range and `gap_start_s` is the end of the preceding
    /// range (or 0.0 when there is no preceding range).
    ///
    /// Mirrors `_getNextRangeIndex` in `GapController.js`.
    fn find_next_range(&self, position_s: f64, buffered: &[TimeRange]) -> Option<(f64, f64)> {
        for (i, range) in buffered.iter().enumerate() {
            if position_s < range.start {
                // End of the preceding buffered range (or stream start).
                let prev_end = if i > 0 { buffered[i - 1].end } else { 0.0 };
                // Only jump when the player is at or past the edge of the
                // previous range (within `threshold_s` of its end).
                if prev_end - position_s < self.settings.threshold_s {
                    return Some((prev_end, range.start));
                }
            }
        }
        None
    }

    /// Return `true` when `position_s` falls inside any of `buffered`.
    fn is_position_buffered(position_s: f64, buffered: &[TimeRange]) -> bool {
        buffered
            .iter()
            .any(|r| position_s >= r.start && position_s <= r.end)
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(pairs: &[(f64, f64)]) -> Vec<TimeRange> {
        pairs.iter().map(|&(s, e)| TimeRange::new(s, e)).collect()
    }

    // ── find_next_range / basic gap detection ─────────────────────────────────

    #[test]
    fn no_seek_when_position_inside_buffered_range() {
        let mut gc = GapController::new();
        let buf = ranges(&[(0.0, 5.0)]);
        assert!(gc.tick(2.0, &buf, false).is_none());
    }

    #[test]
    fn gap_jumped_when_player_is_past_end_of_range() {
        // Player fell off the end of [0, 3]; next range starts at 4.
        let mut gc = GapController::new();
        let buf = ranges(&[(0.0, 3.0), (4.0, 8.0)]);
        // position_s = 3.05 → just past range end; prev_end (3.0) - pos (3.05) = -0.05 < threshold
        let target = gc.tick(3.05, &buf, false);
        assert!(target.is_some());
        let t = target.unwrap();
        // Seek should land at range.start (4.0) + seek_offset (0.1) = 4.1
        assert!((t.position_s - 4.1).abs() < 1e-9);
        assert!(matches!(
            t.reason,
            GapSeekReason::GapJump {
                gap_start_s,
                gap_end_s,
            } if (gap_start_s - 3.0).abs() < 1e-9 && (gap_end_s - 4.0).abs() < 1e-9
        ));
    }

    #[test]
    fn no_gap_jump_when_still_well_inside_first_range() {
        // Player at 2.0 s; range ends at 3.1 s — not near the edge yet.
        let mut gc = GapController::new();
        let buf = ranges(&[(0.0, 3.1), (4.0, 8.0)]);
        // prev_end (3.1) - position (2.0) = 1.1 ≥ threshold → no jump
        assert!(gc.tick(2.0, &buf, false).is_none());
    }

    #[test]
    fn no_gap_jump_when_paused() {
        let mut gc = GapController::new();
        let buf = ranges(&[(0.0, 3.0), (4.0, 8.0)]);
        // Position is in the gap, but paused — no jump.
        assert!(gc.tick(3.5, &buf, true).is_none());
    }

    #[test]
    fn no_gap_jump_during_track_switch() {
        let mut gc = GapController::new();
        gc.on_track_switch_started();
        let buf = ranges(&[(0.0, 3.0), (4.0, 8.0)]);
        assert!(gc.tick(3.5, &buf, false).is_none());
        gc.on_track_switch_ended();
        // Now the jump should fire.
        assert!(gc.tick(3.5, &buf, false).is_some());
    }

    // ── small / large gap threshold ───────────────────────────────────────────

    #[test]
    fn large_gap_jumped_when_jump_large_gaps_true() {
        // Gap is 5 s (> default smallGapLimit 0.8 s) but jump_large_gaps = true.
        let mut gc = GapController::new(); // default: jump_large_gaps = true
        let buf = ranges(&[(0.0, 3.0), (8.0, 12.0)]);
        let target = gc.tick(3.05, &buf, false);
        assert!(target.is_some());
        assert!((target.unwrap().position_s - 8.1).abs() < 1e-9);
    }

    #[test]
    fn large_gap_not_jumped_when_jump_large_gaps_false() {
        let mut gc = GapController::with_settings(GapControllerSettings {
            jump_large_gaps: false,
            ..Default::default()
        });
        let buf = ranges(&[(0.0, 3.0), (8.0, 12.0)]);
        // Gap is 5 s > smallGapLimit (0.8 s) and jumpLargeGaps is disabled.
        assert!(gc.tick(3.05, &buf, false).is_none());
    }

    #[test]
    fn small_gap_always_jumped_regardless_of_jump_large_gaps() {
        let mut gc = GapController::with_settings(GapControllerSettings {
            jump_large_gaps: false,
            small_gap_limit_s: 1.0,
            ..Default::default()
        });
        // Gap is 0.5 s ≤ small_gap_limit → always jumped.
        let buf = ranges(&[(0.0, 3.0), (3.5, 8.0)]);
        let target = gc.tick(3.05, &buf, false);
        assert!(target.is_some());
    }

    // ── stall detection ───────────────────────────────────────────────────────

    #[test]
    fn stall_detected_after_threshold_ticks() {
        // Build a controller with a low threshold so the test is fast.
        let mut gc = GapController::with_settings(GapControllerSettings {
            stall_threshold_ticks: 3,
            stall_seek_s: 0.5,
            ..Default::default()
        });
        // Stalled at 2.0 s inside buffer [0, 5].
        let buf = ranges(&[(0.0, 5.0)]);
        // First few ticks: no action yet.
        assert!(gc.tick(2.0, &buf, false).is_none()); // tick 1 (last_pos = None, no stall yet)
        assert!(gc.tick(2.0, &buf, false).is_none()); // tick 2 (stall_ticks = 1)
        assert!(gc.tick(2.0, &buf, false).is_none()); // tick 3 (stall_ticks = 2)
                                                      // Tick 4: stall_ticks reaches 3 → stall declared → StallRecovery.
        let target = gc.tick(2.0, &buf, false);
        assert!(target.is_some());
        let t = target.unwrap();
        assert_eq!(t.reason, GapSeekReason::StallRecovery);
        assert!((t.position_s - 2.5).abs() < 1e-9); // 2.0 + stall_seek_s (0.5)
    }

    #[test]
    fn stall_counter_resets_when_position_changes() {
        let mut gc = GapController::with_settings(GapControllerSettings {
            stall_threshold_ticks: 3,
            ..Default::default()
        });
        let buf = ranges(&[(0.0, 5.0)]);
        // tick 1: last_pos is None → no stall check; sets last_pos = 2.0.
        gc.tick(2.0, &buf, false);
        // tick 2: stall_ticks → 1 (< 3, not stalled).
        gc.tick(2.0, &buf, false);
        // Position changes → stall_ticks reset to 0.
        gc.tick(3.0, &buf, false);
        // tick 4: stall_ticks → 1 (< 3).
        gc.tick(3.0, &buf, false);
        // tick 5: stall_ticks → 2 (< 3) — not stalled yet.
        assert!(gc.tick(3.0, &buf, false).is_none());
        // tick 6: stall_ticks → 3 (≥ 3) — now stalled; StallRecovery emitted.
        assert!(gc.tick(3.0, &buf, false).is_some());
    }

    #[test]
    fn no_stall_recovery_when_not_buffered() {
        // Stalled but position is NOT inside any buffered range → no recovery.
        let mut gc = GapController::with_settings(GapControllerSettings {
            stall_threshold_ticks: 2,
            ..Default::default()
        });
        let buf = ranges(&[(4.0, 8.0)]); // position 0.5 is not buffered
        gc.tick(0.5, &buf, false);
        gc.tick(0.5, &buf, false); // stall declared but no buffer at 0.5
        assert!(gc.tick(0.5, &buf, false).is_none());
    }

    // ── deduplication ─────────────────────────────────────────────────────────

    #[test]
    fn same_seek_target_not_emitted_twice() {
        let mut gc = GapController::new();
        let buf = ranges(&[(0.0, 3.0), (4.0, 8.0)]);
        let first = gc.tick(3.05, &buf, false);
        assert!(first.is_some());
        // Same position, same gap → should not fire again.
        let second = gc.tick(3.05, &buf, false);
        assert!(second.is_none());
    }

    #[test]
    fn seek_target_allowed_again_after_on_seek() {
        let mut gc = GapController::new();
        let buf = ranges(&[(0.0, 3.0), (4.0, 8.0)]);
        gc.tick(3.05, &buf, false); // emits target
        gc.on_seek(); // simulate seek completed
                      // After on_seek, the same gap can be jumped again if encountered.
        assert!(gc.tick(3.05, &buf, false).is_some());
    }

    // ── edge cases ────────────────────────────────────────────────────────────

    #[test]
    fn empty_buffered_returns_none() {
        let mut gc = GapController::new();
        assert!(gc.tick(2.0, &[], false).is_none());
    }

    #[test]
    fn single_range_no_gap() {
        let mut gc = GapController::new();
        let buf = ranges(&[(0.0, 10.0)]);
        assert!(gc.tick(5.0, &buf, false).is_none());
    }

    #[test]
    fn seek_target_before_position_not_emitted() {
        // Pathological: gap_end < position_s (shouldn't happen, but guard it).
        let mut gc = GapController::new();
        // Build a contrived buffered layout where we manually check the guard.
        let buf = ranges(&[(5.0, 8.0), (9.0, 12.0)]);
        // position_s = 10.0 is inside [9, 12]; no gap ahead of it.
        assert!(gc.tick(10.0, &buf, false).is_none());
    }

    // ── track-switch suppression ──────────────────────────────────────────────

    #[test]
    fn track_switch_suppression_multiple_ticks_then_resumes() {
        let mut gc = GapController::new();
        // A gap between 3.0 and 4.0 would normally trigger a jump.
        let buf = ranges(&[(0.0, 3.0), (4.0, 8.0)]);

        gc.on_track_switch_started();
        // Both ticks while switch is in-progress must be suppressed.
        assert!(gc.tick(3.05, &buf, false).is_none());
        assert!(gc.tick(3.05, &buf, false).is_none());

        gc.on_track_switch_ended();
        // After the switch completes the gap is still there and can now be jumped.
        assert!(gc.tick(3.05, &buf, false).is_some());
    }
}
