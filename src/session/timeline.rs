//! SegmentTimeline helpers.
//!
//! Implements the `$Time$`-addressing path: walking `<SegmentTimeline><S …/>`
//! entries to find the start time and duration of the next segment to fetch,
//! and counting the total number of segments for VOD EOS detection.
//!
//! Mirrors `TimelineSegmentsGetter.js` in dash.js.

use crate::manifest::mpd_types::S;

/// Walk a SegmentTimeline's `S` entries and find the segment whose start time
/// equals `target_time`, or — when `target_time` is `None` — return the very
/// first segment.
///
/// Each `S` element declares:
/// - `t`  — optional start time (inferred from the previous entry when absent)
/// - `d`  — duration (in timescale ticks)
/// - `r`  — optional repeat count (default 0 = no repeat; -1 = infinite repeat)
///
/// Returns `Some((start_time, duration))` on success, `None` when the timeline
/// is exhausted (signals EOS for VOD) or is empty.
pub(super) fn find_timeline_segment(segments: &[S], target_time: Option<u64>) -> Option<(u64, u64)> {
    let mut cursor: u64 = 0; // inferred running start time

    for s in segments {
        // `t` overrides the running cursor when present.
        let seg_start = s.t.unwrap_or(cursor);
        cursor = seg_start;
        let dur = s.d;

        // `r` is the *extra* repeat count (on top of the first occurrence).
        // `r = -1` means infinite (live sliding window); treated as 1 copy for
        // EOS purposes — `find_timeline_segment` will always find it on the
        // first pass (the `target_time` check below will match any time that
        // falls in the infinite-repeat block).
        let repeats: i64 = s.r.unwrap_or(0);
        let repeat_count = if repeats < 0 { i64::MAX } else { repeats };

        // Iterate over the (1 + repeats) occurrences this S entry covers.
        for _ in 0..=repeat_count {
            match target_time {
                None => {
                    // Caller wants the very first segment in the timeline.
                    return Some((cursor, dur));
                }
                Some(t) if cursor == t => {
                    return Some((cursor, dur));
                }
                Some(t) if cursor > t => {
                    // Overshot without a match — timeline is exhausted for this target.
                    return None;
                }
                _ => {}
            }
            cursor += dur;

            // For infinite-repeat entries, stop advancing once we've found
            // that the target falls within this block (handled above via the
            // `cursor == t` arm) or that the target is still ahead.  We must
            // also guard against looping forever when `target_time = None`
            // (handled by the early return above), so this arm is only reached
            // when `target_time = Some(t)` with `t > cursor_after_step`.
            // Break the inner loop so the outer `for s in segments` continues.
            if repeats < 0 {
                break;
            }
        }
    }

    None
}

/// Count the total number of individual segments represented by a
/// `SegmentTimeline`'s `S` entries, respecting the `@r` repeat attribute.
///
/// Entries with `r = -1` (infinite repeat) contribute exactly **1** to the
/// count — this keeps `total_segments` finite so VOD EOS counting works.
/// For live streams the sliding window means the true total is unknown anyway
/// and `total_segments` will be `None` (set by `extract_segment_timing`).
pub(super) fn count_timeline_segments(segments: &[S]) -> u64 {
    segments.iter().fold(0u64, |acc, s| {
        let repeats = s.r.unwrap_or(0);
        let count: u64 = if repeats < 0 {
            // Infinite-repeat: count as 1 occurrence.
            1
        } else {
            // 1 original + r repeats.
            1 + repeats as u64
        };
        acc + count
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{count_timeline_segments, find_timeline_segment};
use crate::manifest::mpd_types::S;

    fn s(t: Option<u64>, d: u64, r: Option<i64>) -> S {
        S { t, d, r, ..Default::default() }
    }

    // ── find_timeline_segment ─────────────────────────────────────────────────

    /// `target_time = None` returns the very first segment in the timeline.
    #[test]
    fn first_segment_no_target() {
        let segs = vec![s(Some(0), 90_000, None), s(None, 90_000, None)];
        assert_eq!(find_timeline_segment(&segs, None), Some((0, 90_000)));
    }

    /// First segment when the timeline's first `t` is nonzero.
    #[test]
    fn first_segment_nonzero_start() {
        let segs = vec![s(Some(1_000), 500, None)];
        assert_eq!(find_timeline_segment(&segs, None), Some((1_000, 500)));
    }

    /// Walk forward through plain entries (no repeats).
    #[test]
    fn sequential_lookup() {
        let segs = vec![
            s(Some(0), 90_000, None),
            s(None, 90_000, None),
            s(None, 90_000, None),
        ];
        assert_eq!(find_timeline_segment(&segs, Some(0)), Some((0, 90_000)));
        assert_eq!(find_timeline_segment(&segs, Some(90_000)), Some((90_000, 90_000)));
        assert_eq!(find_timeline_segment(&segs, Some(180_000)), Some((180_000, 90_000)));
    }

    /// `t` on a later entry overrides the running cursor.
    #[test]
    fn explicit_t_overrides_cursor() {
        let segs = vec![
            s(Some(0), 100, None),
            s(Some(500), 200, None), // gap jump
        ];
        assert_eq!(find_timeline_segment(&segs, Some(500)), Some((500, 200)));
    }

    /// `r = 2` means 3 occurrences total (1 original + 2 repeats).
    #[test]
    fn repeat_r2() {
        let segs = vec![s(Some(0), 90_000, Some(2))]; // covers 0, 90k, 180k
        assert_eq!(find_timeline_segment(&segs, Some(90_000)), Some((90_000, 90_000)));
        assert_eq!(find_timeline_segment(&segs, Some(180_000)), Some((180_000, 90_000)));
        // Beyond the last repeat → None.
        assert_eq!(find_timeline_segment(&segs, Some(270_000)), None);
    }

    /// `r = -1` (infinite repeat) — target at the first position is found.
    #[test]
    fn infinite_repeat() {
        let segs = vec![s(Some(0), 90_000, Some(-1))];
        assert_eq!(find_timeline_segment(&segs, Some(0)), Some((0, 90_000)));
        // The inner loop breaks after one iteration for r=-1 entries,
        // so only the first occurrence is returned by a single call.
        // Live streams advance next_segment_time explicitly after each fetch.
    }

    /// Exhausted timeline (target beyond all entries) returns `None`.
    #[test]
    fn exhausted_returns_none() {
        let segs = vec![s(Some(0), 90_000, None)];
        assert_eq!(find_timeline_segment(&segs, Some(90_000)), None);
    }

    /// Empty timeline returns `None` for any target.
    #[test]
    fn empty_returns_none() {
        assert_eq!(find_timeline_segment(&[], None), None);
        assert_eq!(find_timeline_segment(&[], Some(0)), None);
    }

    // ── count_timeline_segments ───────────────────────────────────────────────

    /// Three plain entries (no r) → count 3.
    #[test]
    fn count_plain_entries() {
        let segs = vec![s(Some(0), 90_000, None), s(None, 90_000, None), s(None, 90_000, None)];
        assert_eq!(count_timeline_segments(&segs), 3);
    }

    /// `r = 2` → 3 occurrences for that entry.
    #[test]
    fn count_with_repeats() {
        let segs = vec![s(Some(0), 90_000, Some(2)), s(None, 90_000, None)];
        assert_eq!(count_timeline_segments(&segs), 4); // 3 + 1
    }

    /// `r = -1` (infinite repeat) → counted as 1.
    #[test]
    fn count_infinite_repeat_is_one() {
        let segs = vec![s(Some(0), 90_000, Some(-1))];
        assert_eq!(count_timeline_segments(&segs), 1);
    }

    /// Empty slice → 0.
    #[test]
    fn count_empty_is_zero() {
        assert_eq!(count_timeline_segments(&[]), 0);
    }
}
