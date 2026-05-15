//! Live-edge segment calculation.
//!
//! Mirrors the live-edge snapping logic in dash.js's `SegmentsUtils.js` /
//! `StreamProcessor.js`: given `MPD@availabilityStartTime` and the current
//! wall clock, compute which segment number to start fetching so that
//! playback begins near (but not past) the live edge.

/// How many segments behind the live edge to start playback when the manifest
/// does not declare `@suggestedPresentationDelay`.  Three segments (~12 s at
/// 4 s/seg) gives enough leeway for CDN propagation without introducing
/// too much latency.
const DEFAULT_LIVE_SAFETY_SEGMENTS: u64 = 3;

#[cfg(not(feature = "std"))]
use crate::math::FloatExt as _;

/// Compute the segment number to start fetching for a live/dynamic stream.
///
/// Uses the DASH spec §5.3.9.4 formula extended with `@availabilityTimeOffset`:
/// ```text
/// live_edge = floor((now − availabilityStartTime + ATO) / segment_duration) + startNumber
/// start_at  = live_edge − safety_segments
/// ```
///
/// `suggested_delay_s` is `MPD@suggestedPresentationDelay` converted to
/// seconds.  When present it overrides the built-in safety margin.
///
/// `availability_time_offset_s` is `SegmentTemplate@availabilityTimeOffset`
/// in seconds (0.0 when absent).  A positive ATO means segments become
/// available `ATO` seconds earlier than their nominal availability time,
/// so the effective "elapsed" window is widened by that amount.  This is
/// used by low-latency CMAF (DASH-LL) profiles to allow fetching partially-
/// produced segments at the live edge.
///
/// Returns `start_number` unchanged when:
/// - `seg_dur_secs ≤ 0` (degenerate template), or
/// - `now_secs < ast_secs` (clock behind AST, should not happen in practice).
pub(super) fn compute_live_edge_segment(
    ast_secs: f64,
    now_secs: f64,
    seg_dur_secs: f64,
    start_number: u64,
    suggested_delay_s: Option<f64>,
    availability_time_offset_s: f64,
) -> u64 {
    if seg_dur_secs <= 0.0 || now_secs <= ast_secs {
        return start_number;
    }
    // ATO shifts the effective "now" forward: segments become available earlier.
    let elapsed = (now_secs - ast_secs + availability_time_offset_s).max(0.0);
    let live_edge = (elapsed / seg_dur_secs).floor() as u64 + start_number;

    let safety = match suggested_delay_s {
        Some(spd) => (spd / seg_dur_secs).ceil() as u64,
        None => DEFAULT_LIVE_SAFETY_SEGMENTS,
    };

    live_edge.saturating_sub(safety).max(start_number)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::compute_live_edge_segment;

    /// Basic: 100 segments have elapsed, no SPD → snap 3 back from live edge.
    #[test]
    fn live_edge_basic_safety_margin() {
        // 4 s segments; 400 s of elapsed time → live_edge = 100 + 1 = 101
        // safety = 3 → start at 98
        let seg = compute_live_edge_segment(0.0, 400.0, 4.0, 1, None, 0.0);
        assert_eq!(seg, 98);
    }

    /// `suggestedPresentationDelay` overrides the default margin.
    #[test]
    fn live_edge_uses_suggested_presentation_delay() {
        // SPD = 12 s, seg_dur = 4 s → safety = ceil(12/4) = 3
        let seg = compute_live_edge_segment(0.0, 400.0, 4.0, 1, Some(12.0), 0.0);
        assert_eq!(seg, 98);
        // SPD = 8 s → safety = 2
        let seg = compute_live_edge_segment(0.0, 400.0, 4.0, 1, Some(8.0), 0.0);
        assert_eq!(seg, 99);
    }

    /// Non-zero `startNumber` is included in the result.
    #[test]
    fn live_edge_non_zero_start_number() {
        // start_number = 5; live_edge = floor(400/4) + 5 = 105; safety=3 → 102
        let seg = compute_live_edge_segment(0.0, 400.0, 4.0, 5, None, 0.0);
        assert_eq!(seg, 102);
    }

    /// Non-zero `availabilityStartTime` (e.g. BBC: AST = epoch + 25 s).
    #[test]
    fn live_edge_with_nonzero_ast() {
        // AST = 25 s, now = 425 s → elapsed = 400 s, live_edge = 100 + 1 = 101; safety=3 → 98
        let seg = compute_live_edge_segment(25.0, 425.0, 4.0, 1, None, 0.0);
        assert_eq!(seg, 98);
    }

    /// Result is clamped to at least `start_number` (no underflow).
    #[test]
    fn live_edge_clamps_to_start_number() {
        // Only 2 segments elapsed → live_edge = 3; safety = 3 → would underflow to 0
        // clamped to start_number (1)
        let seg = compute_live_edge_segment(0.0, 8.0, 4.0, 1, None, 0.0);
        assert_eq!(seg, 1);
    }

    /// `now < ast` (clock behind availability) → return `start_number`.
    #[test]
    fn live_edge_future_ast_returns_start_number() {
        let seg = compute_live_edge_segment(1000.0, 500.0, 4.0, 1, None, 0.0);
        assert_eq!(seg, 1);
    }

    /// Degenerate `seg_dur = 0` → return `start_number` without panicking.
    #[test]
    fn live_edge_zero_duration_returns_start_number() {
        let seg = compute_live_edge_segment(0.0, 400.0, 0.0, 1, None, 0.0);
        assert_eq!(seg, 1);
    }

    /// `availabilityTimeOffset` widens the live-edge window by that many seconds.
    ///
    /// With `now=401`, ATO=3.5 s and 4 s segments:
    /// - Without ATO: floor(401/4) + 1 = 100 + 1 = 101; safety=3 → 98.
    /// - With ATO=3.5: floor(404.5/4) + 1 = 101 + 1 = 102; safety=3 → 99.
    #[test]
    fn live_edge_with_availability_time_offset() {
        // Without ATO baseline.
        let seg_no_ato = compute_live_edge_segment(0.0, 401.0, 4.0, 1, None, 0.0);
        assert_eq!(seg_no_ato, 98);
        // With ATO=3.5 s → one segment further ahead.
        let seg = compute_live_edge_segment(0.0, 401.0, 4.0, 1, None, 3.5);
        assert_eq!(seg, 99);
    }

    /// ATO equal to a full segment duration allows the currently-being-produced
    /// segment to be included in the fetch window.
    #[test]
    fn live_edge_ato_full_segment_duration() {
        // 4 s segments, ATO=4 s → effective elapsed = 404 s
        // live_edge = floor(404/4) + 1 = 101 + 1 = 102; safety=3 → 99
        let seg = compute_live_edge_segment(0.0, 400.0, 4.0, 1, None, 4.0);
        assert_eq!(seg, 99);
    }
}
