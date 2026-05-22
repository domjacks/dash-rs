//! SegmentTimeline helpers.
//!
//! Implements the `$Time$`-addressing path: walking `<SegmentTimeline><S …/>`
//! entries to find the start time and duration of the next segment to fetch,
//! and counting the total number of segments for VOD EOS detection.
//!
//! Mirrors `TimelineSegmentsGetter.js` in dash.js.

use crate::manifest::mpd_types::S;

/// Snap `target_ticks` down to the nearest grid-aligned segment start time
/// within the SegmentTimeline.
///
/// Used during live-edge snapping (session construction) to set
/// `TrackState::next_segment_time` so that the first fetch lands at the
/// live edge rather than at the beginning of the time-shift window (which is
/// what `find_timeline_segment(None)` would return).
///
/// For infinite-repeat entries (`r = -1`) this returns the largest
/// `entry.t + k × entry.d ≤ target_ticks`.  For finite-repeat entries an
/// exact match is required (partial "snap" is not meaningful for discrete
/// lists of specific timestamps).
///
/// Returns `None` when `target_ticks` falls before any timeline entry, the
/// timeline is empty, or no entry covers the target.
pub(super) fn snap_to_timeline_grid(segments: &[S], target_ticks: u64) -> Option<u64> {
    let mut cursor: u64 = 0;

    for (idx, s) in segments.iter().enumerate() {
        let seg_start = s.t.unwrap_or(cursor);
        cursor = seg_start;
        let dur = s.d;
        if dur == 0 {
            continue;
        }
        let repeats: i64 = s.r.unwrap_or(0);

        if repeats < 0 {
            // ── Infinite-repeat entry (r = -1) ───────────────────────────────
            // Snap down to the largest aligned position ≤ target_ticks.
            if target_ticks < cursor {
                return None; // target is before this block
            }
            // Check upper bound: does the next S entry have an explicit `t`?
            let next_explicit_t: Option<u64> = segments.get(idx + 1).and_then(|n| n.t);
            if let Some(nt) = next_explicit_t {
                if target_ticks >= nt {
                    cursor = nt;
                    continue; // target belongs to a later block
                }
            }
            // Snap down: largest `cursor + k * dur ≤ target_ticks`.
            let steps = (target_ticks - cursor) / dur;
            return Some(cursor + steps * dur);
        } else {
            // ── Finite-repeat entry (r ≥ 0) ──────────────────────────────────
            for _ in 0..=repeats as u64 {
                if cursor == target_ticks {
                    return Some(cursor);
                }
                if cursor > target_ticks {
                    return None; // overshot
                }
                cursor += dur;
            }
        }
    }
    None
}

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
pub(super) fn find_timeline_segment(
    segments: &[S],
    target_time: Option<u64>,
) -> Option<(u64, u64)> {
    let mut cursor: u64 = 0; // inferred running start time

    for (idx, s) in segments.iter().enumerate() {
        // `t` overrides the running cursor when present.
        let seg_start = s.t.unwrap_or(cursor);
        cursor = seg_start;
        let dur = s.d;

        // `r` is the *extra* repeat count (on top of the first occurrence).
        // `r = -1` means infinite (live sliding window).
        let repeats: i64 = s.r.unwrap_or(0);

        if repeats < 0 {
            // ── Infinite-repeat entry (r = -1) ───────────────────────────────
            // Use arithmetic snapping to avoid an unbounded inner loop.
            // The block's upper bound is the explicit `t` of the next S entry,
            // when present — targets at or beyond that point belong to the
            // subsequent entry.
            let next_explicit_t: Option<u64> = segments.get(idx + 1).and_then(|next| next.t);

            match target_time {
                None => {
                    // Caller wants the very first segment — return immediately.
                    return Some((cursor, dur));
                }
                Some(t) => {
                    if t < cursor {
                        // Overshot without a match.
                        return None;
                    }
                    if dur == 0 {
                        // Degenerate duration — cannot advance; let subsequent
                        // S entries be checked.
                        break;
                    }
                    // If the next S entry has an explicit `t` and the requested
                    // time falls at or beyond it, skip this block entirely.
                    if let Some(nt) = next_explicit_t {
                        if t >= nt {
                            // Advance cursor to the start of the next block
                            // so its implicit-cursor path (no `t`) is correct.
                            cursor = nt;
                            continue;
                        }
                    }
                    let offset = t - cursor;
                    if offset.is_multiple_of(dur) {
                        // t falls exactly on this entry's grid → match.
                        return Some((t, dur));
                    }
                    // t is not aligned to this entry's grid.  Advance cursor
                    // past t so that any subsequent S entry can be checked.
                    let steps = offset / dur + 1;
                    cursor += steps * dur;
                }
            }
        } else {
            // ── Finite-repeat entry (r ≥ 0) ──────────────────────────────────
            // Iterate over the (1 + r) occurrences this S entry covers.
            for _ in 0..=repeats as u64 {
                match target_time {
                    None => {
                        return Some((cursor, dur));
                    }
                    Some(t) if cursor == t => {
                        return Some((cursor, dur));
                    }
                    Some(t) if cursor > t => {
                        // Overshot without a match.
                        return None;
                    }
                    _ => {}
                }
                cursor += dur;
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
    use super::{count_timeline_segments, find_timeline_segment, snap_to_timeline_grid};
    use crate::manifest::mpd_types::S;
    use alloc::vec;

    fn s(t: Option<u64>, d: u64, r: Option<i64>) -> S {
        S {
            t,
            d,
            r,
            ..Default::default()
        }
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
        assert_eq!(
            find_timeline_segment(&segs, Some(90_000)),
            Some((90_000, 90_000))
        );
        assert_eq!(
            find_timeline_segment(&segs, Some(180_000)),
            Some((180_000, 90_000))
        );
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
        assert_eq!(
            find_timeline_segment(&segs, Some(90_000)),
            Some((90_000, 90_000))
        );
        assert_eq!(
            find_timeline_segment(&segs, Some(180_000)),
            Some((180_000, 90_000))
        );
        // Beyond the last repeat → None.
        assert_eq!(find_timeline_segment(&segs, Some(270_000)), None);
    }

    /// `r = -1` (infinite repeat) — any target on the entry's grid is found.
    ///
    /// Live streams call `find_timeline_segment` once per segment, each time
    /// passing `next_segment_time = previous_start + duration`.  The arithmetic
    /// snapping must therefore handle targets arbitrarily far into the future.
    #[test]
    fn infinite_repeat() {
        let segs = vec![s(Some(0), 90_000, Some(-1))];
        // First occurrence.
        assert_eq!(find_timeline_segment(&segs, Some(0)), Some((0, 90_000)));
        // Second occurrence (simulates next_segment_time = 0 + 90_000).
        assert_eq!(
            find_timeline_segment(&segs, Some(90_000)),
            Some((90_000, 90_000))
        );
        // Far-future occurrence (simulates many segments later).
        assert_eq!(
            find_timeline_segment(&segs, Some(90_000 * 5)),
            Some((90_000 * 5, 90_000))
        );
        // Misaligned target returns None.
        assert_eq!(find_timeline_segment(&segs, Some(1)), None);
    }

    /// `r = -1` block followed by another `S` entry — targets in both ranges
    /// are found correctly, and the boundary belongs to the second entry.
    #[test]
    fn infinite_repeat_then_next_entry() {
        // r=-1 block: t=0, d=100 covers t=0, 100, 200, 300, 400 (i.e. [0, 500)).
        // Second block: t=500, d=200.
        let segs = vec![s(Some(0), 100, Some(-1)), s(Some(500), 200, None)];
        assert_eq!(find_timeline_segment(&segs, Some(0)), Some((0, 100)));
        assert_eq!(find_timeline_segment(&segs, Some(300)), Some((300, 100)));
        // t=500 belongs to the second S entry, not the r=-1 block.
        assert_eq!(find_timeline_segment(&segs, Some(500)), Some((500, 200)));
        // t=700 is beyond the second entry (500+200) → None.
        assert_eq!(find_timeline_segment(&segs, Some(700)), None);
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

    // ── snap_to_timeline_grid ─────────────────────────────────────────────────

    /// Exact-match on a finite entry returns that entry's start time.
    #[test]
    fn snap_exact_match_finite() {
        let segs = vec![s(Some(0), 90_000, None), s(None, 90_000, None)];
        assert_eq!(snap_to_timeline_grid(&segs, 0), Some(0));
        assert_eq!(snap_to_timeline_grid(&segs, 90_000), Some(90_000));
    }

    /// Target before any entry returns `None`.
    #[test]
    fn snap_before_all_entries_returns_none() {
        let segs = vec![s(Some(1_000), 500, None)];
        assert_eq!(snap_to_timeline_grid(&segs, 0), None);
    }

    /// Empty timeline always returns `None`.
    #[test]
    fn snap_empty_returns_none() {
        assert_eq!(snap_to_timeline_grid(&[], 12_345), None);
    }

    /// Infinite-repeat entry: target exactly on grid → returns target.
    #[test]
    fn snap_infinite_repeat_exact() {
        let segs = vec![s(Some(0), 90_000, Some(-1))];
        assert_eq!(snap_to_timeline_grid(&segs, 0), Some(0));
        assert_eq!(snap_to_timeline_grid(&segs, 90_000), Some(90_000));
        assert_eq!(snap_to_timeline_grid(&segs, 270_000), Some(270_000));
    }

    /// Infinite-repeat: misaligned target snaps DOWN to nearest grid position.
    #[test]
    fn snap_infinite_repeat_misaligned_snaps_down() {
        let segs = vec![s(Some(0), 90_000, Some(-1))];
        // 1 tick past grid → snaps to 0.
        assert_eq!(snap_to_timeline_grid(&segs, 1), Some(0));
        // 89_999 ticks in → snaps to 0.
        assert_eq!(snap_to_timeline_grid(&segs, 89_999), Some(0));
        // 90_001 → snaps to 90_000.
        assert_eq!(snap_to_timeline_grid(&segs, 90_001), Some(90_000));
        // 200_000 → snaps to 180_000.
        assert_eq!(snap_to_timeline_grid(&segs, 200_000), Some(180_000));
    }

    /// BBC-style epoch stream: large absolute timestamps snap correctly.
    ///
    /// Simulates an `r=-1` entry starting at a large epoch-scale `t` value
    /// (analogous to `availabilityStartTime = 1970-01-01`, timescale=48000,
    /// seg_dur=184320 ticks).  A slightly-off target must snap down to the
    /// nearest aligned value so that `find_timeline_segment` can find it.
    #[test]
    fn snap_epoch_scale_r_minus_one() {
        let t_start: u64 = 83_855_000_000_000; // epoch-scale start of live window
        let d: u64 = 184_320; // BBC segment duration in ticks
        let segs = vec![s(Some(t_start), d, Some(-1))];

        // Exact aligned target.
        let aligned = t_start + 1000 * d;
        assert_eq!(snap_to_timeline_grid(&segs, aligned), Some(aligned));

        // Target 100 ticks past an aligned position → snaps down.
        let off_by_100 = t_start + 1000 * d + 100;
        assert_eq!(snap_to_timeline_grid(&segs, off_by_100), Some(aligned));

        // Target just before the live window → None.
        assert_eq!(snap_to_timeline_grid(&segs, t_start - 1), None);
    }

    /// `r=-1` followed by an explicit next entry: target in the r=-1 range
    /// snaps within that block; target at or past the next `t` belongs to the
    /// next block.
    #[test]
    fn snap_infinite_repeat_with_next_block() {
        let segs = vec![s(Some(0), 100, Some(-1)), s(Some(500), 200, None)];
        // 450 → snaps to 400 (within r=-1 block, < 500).
        assert_eq!(snap_to_timeline_grid(&segs, 450), Some(400));
        // 500 → belongs to the second block (exact match).
        assert_eq!(snap_to_timeline_grid(&segs, 500), Some(500));
        // 600 → beyond the second entry (only has d=200 at t=500) → None.
        assert_eq!(snap_to_timeline_grid(&segs, 600), None);
    }

    // ── count_timeline_segments ───────────────────────────────────────────────

    /// Three plain entries (no r) → count 3.
    #[test]
    fn count_plain_entries() {
        let segs = vec![
            s(Some(0), 90_000, None),
            s(None, 90_000, None),
            s(None, 90_000, None),
        ];
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
