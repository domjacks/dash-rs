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
            let next_explicit_t: Option<u64> = segments
                .get(idx + 1)
                .and_then(|next| next.t);

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
        assert_eq!(find_timeline_segment(&segs, Some(90_000)), Some((90_000, 90_000)));
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
