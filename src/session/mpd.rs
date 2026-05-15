//! MPD manifest helpers.
//!
//! Extracts and merges the `SegmentTemplate` hierarchy, enumerates
//! `Representation`s, and derives per-track timing parameters from the MPD.
//!
//! Mirrors `MpdHelper.js`, `DashManifestModel.js`, and parts of
//! `TemplateSegmentsGetter.js` in dash.js.

use crate::manifest::mpd_types::{
    AdaptationSet, Period, Representation, SegmentBase, SegmentList, SegmentTemplate,
};

use super::timeline::count_timeline_segments;
use super::MediaType;
use crate::abr::RepresentationInfo;

#[cfg(not(feature = "std"))]
use crate::math::FloatExt as _;
#[cfg(not(feature = "std"))]
use alloc::{
    borrow::ToOwned,
    string::{String, ToString},
    vec::Vec,
};

// ── Public helpers ────────────────────────────────────────────────────────────

/// Merge `SegmentTemplate` attributes across the DASH element hierarchy.
///
/// DASH §5.3.9.4 specifies that attributes not set at a lower level shall be
/// inherited from the nearest parent element that defines them.  The
/// resolution order is Representation (most specific) → AdaptationSet → Period
/// (least specific).
///
/// Returns an **owned** merged template so the caller can hold it without
/// lifetime ties to multiple levels of the MPD tree.  Returns `None` when no
/// `SegmentTemplate` exists at any level.
pub(super) fn merge_segment_templates(
    repr: Option<&Representation>,
    adaptation: &AdaptationSet,
    period: &Period,
) -> Option<SegmentTemplate> {
    let sources: [Option<&SegmentTemplate>; 3] = [
        repr.and_then(|r| r.SegmentTemplate.as_ref()),
        adaptation.SegmentTemplate.as_ref(),
        period.SegmentTemplate.as_ref(),
    ];

    if sources.iter().all(|s| s.is_none()) {
        return None;
    }

    // For each attribute: first source that has a value wins.
    let mut merged = SegmentTemplate::default();
    for tmpl in sources.iter().flatten() {
        if merged.media.is_none() {
            merged.media.clone_from(&tmpl.media);
        }
        if merged.initialization.is_none() {
            merged.initialization.clone_from(&tmpl.initialization);
        }
        if merged.startNumber.is_none() {
            merged.startNumber = tmpl.startNumber;
        }
        if merged.duration.is_none() {
            merged.duration = tmpl.duration;
        }
        if merged.timescale.is_none() {
            merged.timescale = tmpl.timescale;
        }
        if merged.presentationTimeOffset.is_none() {
            merged.presentationTimeOffset = tmpl.presentationTimeOffset;
        }
        if merged.availabilityTimeOffset.is_none() {
            merged.availabilityTimeOffset = tmpl.availabilityTimeOffset;
        }
        if merged.availabilityTimeComplete.is_none() {
            merged.availabilityTimeComplete = tmpl.availabilityTimeComplete;
        }
        if merged.Initialization.is_none() {
            merged.Initialization.clone_from(&tmpl.Initialization);
        }
        if merged.SegmentTimeline.is_none() {
            merged.SegmentTimeline.clone_from(&tmpl.SegmentTimeline);
        }
    }

    Some(merged)
}

/// Returns `true` when `adaptation` is a video track.
///
/// Checks `@contentType` first; falls back to the `@mimeType` attribute of
/// the `AdaptationSet` and its `Representation` children.
fn is_video_adaptation(a: &AdaptationSet) -> bool {
    a.contentType.as_deref() == Some("video")
        || a.mimeType
            .as_deref()
            .is_some_and(|m| m.starts_with("video/"))
        || a.representations.iter().any(|r| {
            r.mimeType
                .as_deref()
                .is_some_and(|m| m.starts_with("video/"))
        })
}

/// Returns `true` when `adaptation` is an audio track.
fn is_audio_adaptation(a: &AdaptationSet) -> bool {
    a.contentType.as_deref() == Some("audio")
        || a.mimeType
            .as_deref()
            .is_some_and(|m| m.starts_with("audio/"))
        || a.representations.iter().any(|r| {
            r.mimeType
                .as_deref()
                .is_some_and(|m| m.starts_with("audio/"))
        })
}

/// Return a reference to the best-matching adaptation set for `media_type`
/// within `period`.
///
/// **Video**: returns the first video `AdaptationSet` (there is almost always
/// only one).
///
/// **Audio**: when multiple audio `AdaptationSet`s are present (e.g. a stream
/// that offers both AAC-HE at low bitrates and AAC-LC at high bitrates), the
/// one whose representations have the **highest maximum bandwidth** is chosen.
/// This ensures that high-quality codec tiers (e.g. `mp4a.40.2` at 320 kbps)
/// are preferred over lower-quality fallback tiers (e.g. `mp4a.40.5` at
/// 96 kbps).  When all candidates have equal or unknown bandwidths, the first
/// matching `AdaptationSet` is returned (preserving prior behaviour).
pub(super) fn find_adaptation<'a>(
    period: &'a Period,
    media_type: &MediaType,
) -> Option<&'a AdaptationSet> {
    match media_type {
        MediaType::Video => period.adaptations.iter().find(|a| is_video_adaptation(a)),
        MediaType::Audio => period
            .adaptations
            .iter()
            .filter(|a| is_audio_adaptation(a))
            .enumerate()
            .max_by_key(|(i, a)| {
                let bw = a
                    .representations
                    .iter()
                    .filter_map(|r| r.bandwidth)
                    .max()
                    .unwrap_or(0);
                // Secondary key: `usize::MAX - i` so that when bandwidths are
                // equal the first AdaptationSet in document order wins.
                // (The main audio always precedes alternate / audio-description
                // tracks in well-formed DASH manifests.)
                (bw, usize::MAX - i)
            })
            .map(|(_, a)| a),
    }
}

/// Extract `(start_number, segment_duration_ms, total_segments)` from the
/// `SegmentTemplate` that governs `adaptation`, walking up to `period` if
/// needed.  `presentation_duration` is `MPD@mediaPresentationDuration`.
///
/// `start_number` defaults to 1 when `@startNumber` is absent (DASH spec §5.3.9.2).
/// `segment_duration_ms` defaults to 2 000 ms when `@duration` / `@timescale`
/// are absent (safe fallback that keeps the buffer estimator conservative).
/// `total_segments` is derived from `presentation_duration` when set;
/// falls back to `Period@duration` when the MPD-level attribute is absent;
/// is `None` only when both are absent (e.g. live / dynamic stream).
pub(super) fn extract_segment_timing(
    adaptation: &AdaptationSet,
    period: &Period,
    presentation_duration: Option<core::time::Duration>,
) -> (u64, f64, Option<u64>) {
    // Use the first representation as a sample; all representations within
    // a track should have the same effective segment duration.
    let first_repr = adaptation.representations.first();
    let Some(t) = merge_segment_templates(first_repr, adaptation, period) else {
        return (1, 2_000.0, None);
    };

    // Warn if sibling representations declare inconsistent effective durations.
    if adaptation.representations.len() > 1 {
        let effective_dur_s = |r: &Representation| -> Option<f64> {
            merge_segment_templates(Some(r), adaptation, period)
                .and_then(|m| Some(m.duration? / m.timescale.unwrap_or(1) as f64))
        };
        let first_dur = first_repr.and_then(effective_dur_s);
        for r in adaptation.representations.iter().skip(1) {
            match (first_dur, effective_dur_s(r)) {
                (Some(fd), Some(rd)) if (fd - rd).abs() > 1e-9 => {
                    tracing::warn!(
                        "AdaptationSet representations have inconsistent segment durations \
                         ({fd} s vs {rd} s); using first representation as representative"
                    );
                    break;
                }
                _ => {}
            }
        }
    }

    let start_number = t.startNumber.unwrap_or(1);

    // For SegmentTimeline streams the fixed @duration attribute is absent;
    // derive segment_duration_ms from the first S entry instead.
    let seg_dur_ms = match (t.duration, t.timescale) {
        (Some(dur), Some(ts)) => dur / (ts as f64) * 1_000.0,
        (Some(dur), None) => dur * 1_000.0, // timescale defaults to 1
        _ => t
            .SegmentTimeline
            .as_ref()
            .and_then(|tl| tl.segments.first())
            .map(|s| {
                let ts = t.timescale.unwrap_or(1) as f64;
                s.d as f64 / ts * 1_000.0
            })
            .unwrap_or(2_000.0),
    };

    // For SegmentTimeline streams derive total_segments from the timeline count.
    // For live streams (no mediaPresentationDuration) this will be None.
    let total_segments = if let Some(tl) = &t.SegmentTimeline {
        // If any entry has r=-1 (infinite repeat) the timeline is unbounded.
        // EOS is determined by availabilityEndTime or segment fetch failure,
        // not by segment count — return None so the count-based EOS guard is
        // not triggered.
        let has_infinite_repeat = tl.segments.iter().any(|s| s.r.unwrap_or(0) < 0);
        if has_infinite_repeat {
            None
        } else {
            let count = count_timeline_segments(&tl.segments);
            if count > 0 {
                Some(count)
            } else {
                None
            }
        }
    } else {
        let effective_duration = period.duration.or(presentation_duration);
        effective_duration.map(|d| {
            let total_ms = d.as_millis() as f64;
            (total_ms / seg_dur_ms).ceil() as u64
        })
    };

    (start_number, seg_dur_ms, total_segments)
}

/// Build a sorted `Vec<RepresentationInfo>` from an `AdaptationSet`.
///
/// Representations without a `@bandwidth` attribute are skipped.
/// The result is sorted ascending by bandwidth so index 0 is always the
/// lowest quality — required by all ABR rules.
pub(super) fn extract_representations(adaptation: &AdaptationSet) -> Vec<RepresentationInfo> {
    let mut reps: Vec<RepresentationInfo> = adaptation
        .representations
        .iter()
        .filter_map(|r| {
            let bandwidth = r.bandwidth?;
            Some(RepresentationInfo {
                id: r.id.clone().unwrap_or_default(),
                bandwidth,
                width: r.width.map(|w| w as u32),
                height: r.height.map(|h| h as u32),
            })
        })
        .collect();

    reps.sort_by_key(|r| r.bandwidth);
    reps
}

/// Derive a MIME type string suitable for `addSourceBuffer` from an
/// [`AdaptationSet`].
///
/// Resolution order (DASH spec §5.3.7.2):
/// 1. `AdaptationSet@mimeType` combined with the best available codec string:
///    - `AdaptationSet@codecs` (shared across all representations), or
///    - the first `Representation@codecs` found in the set.
/// 2. The first `Representation` that declares its own `@mimeType` (plus its
///    `@codecs` if present).
/// 3. `"application/octet-stream"` as a last-resort fallback.
///
/// Searching *all* representations (not just the first) ensures the codec
/// string is found even when lower-bandwidth representations omit it.
pub(super) fn adaptation_mime_type(adaptation: &AdaptationSet) -> String {
    // Helper: first codec string declared in any representation.
    let rep_codecs = || {
        adaptation
            .representations
            .iter()
            .find_map(|r| r.codecs.as_deref())
    };

    if let Some(mime) = &adaptation.mimeType {
        // Prefer AS-level @codecs; fall back to first Representation that has them.
        let codecs = adaptation.codecs.as_deref().or_else(rep_codecs);
        return match codecs {
            Some(c) => format!("{};codecs=\"{}\"", mime, c),
            None => mime.clone(),
        };
    }

    // No AS-level MIME: use the first Representation that declares one.
    for rep in &adaptation.representations {
        if let Some(mime) = &rep.mimeType {
            return match &rep.codecs {
                Some(c) => format!("{};codecs=\"{}\"", mime, c),
                None => mime.clone(),
            };
        }
    }

    "application/octet-stream".to_owned()
}

/// Merge `SegmentList` attributes across the DASH element hierarchy.
///
/// Resolution order: Representation (most specific) → AdaptationSet → Period.
/// `duration`, `timescale`, and `Initialization` are inherited from the nearest
/// parent that defines them; `segment_urls` are taken from the **innermost**
/// level that declares a non-empty list (they are never concatenated across
/// levels).
///
/// Returns `None` when no `SegmentList` is present at any level.
pub(super) fn merge_segment_lists(
    repr: Option<&Representation>,
    adaptation: &AdaptationSet,
    period: &Period,
) -> Option<SegmentList> {
    let sources: [Option<&SegmentList>; 3] = [
        repr.and_then(|r| r.SegmentList.as_ref()),
        adaptation.SegmentList.as_ref(),
        period.SegmentList.as_ref(),
    ];

    if sources.iter().all(|s| s.is_none()) {
        return None;
    }

    let mut merged = SegmentList::default();
    for sl in sources.iter().flatten() {
        if merged.duration.is_none() {
            merged.duration = sl.duration;
        }
        if merged.timescale.is_none() {
            merged.timescale = sl.timescale;
        }
        if merged.Initialization.is_none() {
            merged.Initialization.clone_from(&sl.Initialization);
        }
        // Take segment_urls from the innermost non-empty list only.
        if merged.segment_urls.is_empty() && !sl.segment_urls.is_empty() {
            merged.segment_urls.clone_from(&sl.segment_urls);
        }
    }

    Some(merged)
}

/// Extract `(start_number, segment_duration_ms, total_segments)` for a track
/// that uses `SegmentList` addressing.
///
/// `start_number` is taken from `SegmentList@startNumber` (default 0).
/// The segment index into `segment_urls` is computed as
/// `segment_number − start_number`, so a `startNumber="1"` SegmentList
/// works just like a `SegmentTemplate` with the same `startNumber`.
/// `total_segments` is `segment_urls.len()` (or `None` when the merged list
/// is empty).
pub(super) fn extract_segment_list_timing(
    adaptation: &AdaptationSet,
    period: &Period,
) -> (u64, f64, Option<u64>) {
    let first_repr = adaptation.representations.first();
    let Some(sl) = merge_segment_lists(first_repr, adaptation, period) else {
        return (0, 2_000.0, None);
    };

    let start_number = sl.startNumber.unwrap_or(0);

    let seg_dur_ms = match (sl.duration, sl.timescale) {
        (Some(dur), Some(ts)) => dur as f64 / ts as f64 * 1_000.0,
        (Some(dur), None) => dur as f64 * 1_000.0, // timescale defaults to 1
        _ => 2_000.0,
    };

    let total = if sl.segment_urls.is_empty() {
        None
    } else {
        Some(sl.segment_urls.len() as u64)
    };

    (start_number, seg_dur_ms, total)
}

/// Walk the element hierarchy (Representation → AdaptationSet → Period) for
/// the first `SegmentBase` element, returning a reference to it.
///
/// Returns `None` when no `SegmentBase` is found at any level.
pub(super) fn find_segment_base<'a>(
    repr: Option<&'a Representation>,
    adaptation: &'a AdaptationSet,
    period: &'a Period,
) -> Option<&'a SegmentBase> {
    repr.and_then(|r| r.SegmentBase.as_ref())
        .or(adaptation.SegmentBase.as_ref())
        .or(period.SegmentBase.as_ref())
}

/// Return the relative media-file path for a `SegmentBase` track.
///
/// For `SegmentBase`, the media file *is* the `BaseURL` element at the
/// representation (or adaptation) level.  Returns an empty string when no
/// `BaseURL` is declared — callers should treat the effective base URL itself
/// as the media file.
pub(super) fn segment_base_media_path(
    repr: Option<&Representation>,
    adaptation: &AdaptationSet,
) -> String {
    repr.and_then(|r| r.BaseURL.first())
        .or_else(|| adaptation.BaseURL.first())
        .map(|b| b.base.clone())
        .unwrap_or_default()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::merge_segment_templates;
    use crate::manifest::mpd_types::{AdaptationSet, Period, Representation, SegmentTemplate};

    /// Template present only at the Representation level.
    #[test]
    fn merge_repr_level_only() {
        let mut repr = Representation::default();
        repr.SegmentTemplate = Some(SegmentTemplate {
            media: Some("video-$RepresentationID$-$Number$.m4s".into()),
            initialization: Some("video-$RepresentationID$-init.mp4".into()),
            timescale: Some(25_000),
            duration: Some(96_000.0),
            ..Default::default()
        });

        let merged =
            merge_segment_templates(Some(&repr), &AdaptationSet::default(), &Period::default())
                .unwrap();

        assert_eq!(
            merged.media.as_deref(),
            Some("video-$RepresentationID$-$Number$.m4s")
        );
        assert_eq!(merged.timescale, Some(25_000));
        assert_eq!(merged.duration, Some(96_000.0));
        assert_eq!(merged.startNumber, None);
    }

    /// `startNumber` lives on the AdaptationSet; Representation wins for `timescale`.
    #[test]
    fn merge_inherits_from_adaptation_set() {
        let mut repr = Representation::default();
        repr.SegmentTemplate = Some(SegmentTemplate {
            media: Some("seg-$Number$.m4s".into()),
            timescale: Some(48_000),
            duration: Some(192_000.0),
            ..Default::default()
        });

        let mut adaptation = AdaptationSet::default();
        adaptation.SegmentTemplate = Some(SegmentTemplate {
            startNumber: Some(5),
            timescale: Some(1_000),
            ..Default::default()
        });

        let merged = merge_segment_templates(Some(&repr), &adaptation, &Period::default()).unwrap();

        assert_eq!(merged.media.as_deref(), Some("seg-$Number$.m4s"));
        assert_eq!(merged.timescale, Some(48_000));
        assert_eq!(merged.startNumber, Some(5));
    }

    /// Period-level fallback: template only on Period, none lower.
    #[test]
    fn merge_period_level_fallback() {
        let mut period = Period::default();
        period.SegmentTemplate = Some(SegmentTemplate {
            media: Some("s$Number$.m4s".into()),
            timescale: Some(1_000),
            duration: Some(2_000.0),
            ..Default::default()
        });

        let merged = merge_segment_templates(None, &AdaptationSet::default(), &period).unwrap();

        assert_eq!(merged.media.as_deref(), Some("s$Number$.m4s"));
        assert_eq!(merged.timescale, Some(1_000));
    }

    /// No SegmentTemplate at any level → `None`.
    #[test]
    fn merge_no_template_returns_none() {
        assert!(merge_segment_templates(
            Some(&Representation::default()),
            &AdaptationSet::default(),
            &Period::default()
        )
        .is_none());
        assert!(
            merge_segment_templates(None, &AdaptationSet::default(), &Period::default()).is_none()
        );
    }
}

// ── Extended tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod extended_tests {
    use super::*;
    use crate::manifest::mpd_types::{
        AdaptationSet, BaseURL, Initialization, Period, Representation, SegmentBase, SegmentList,
        SegmentTemplate, SegmentTimeline, SegmentURL, S,
    };
    use crate::session::MediaType;

    // ── find_adaptation ───────────────────────────────────────────────────────

    fn make_adaptation(content_type: &str, bandwidths: &[u64]) -> AdaptationSet {
        let mut a = AdaptationSet::default();
        a.contentType = Some(content_type.to_owned());
        a.representations = bandwidths
            .iter()
            .map(|&bw| {
                let mut r = Representation::default();
                r.bandwidth = Some(bw);
                r
            })
            .collect();
        a
    }

    #[test]
    fn find_adaptation_returns_first_video() {
        let mut period = Period::default();
        period.adaptations = vec![
            make_adaptation("audio", &[128_000]),
            make_adaptation("video", &[1_000_000, 4_000_000]),
        ];
        let found = find_adaptation(&period, &MediaType::Video);
        assert!(found.is_some());
        assert_eq!(found.unwrap().contentType.as_deref(), Some("video"));
    }

    #[test]
    fn find_adaptation_returns_none_when_absent() {
        let mut period = Period::default();
        period.adaptations = vec![make_adaptation("video", &[1_000_000])];
        assert!(find_adaptation(&period, &MediaType::Audio).is_none());
    }

    #[test]
    fn find_adaptation_audio_picks_highest_bandwidth() {
        // Two audio AdaptationSets: low quality (96 kbps) and high quality (320 kbps).
        let low = make_adaptation("audio", &[64_000, 96_000]);
        let high = make_adaptation("audio", &[192_000, 320_000]);
        let mut period = Period::default();
        period.adaptations = vec![low, high];

        let found = find_adaptation(&period, &MediaType::Audio).unwrap();
        assert_eq!(
            found
                .representations
                .iter()
                .map(|r| r.bandwidth.unwrap())
                .max(),
            Some(320_000),
            "should pick the higher-bandwidth audio AdaptationSet"
        );
    }

    #[test]
    fn find_adaptation_audio_tie_break_first_in_document_order() {
        // Both AdaptationSets have identical max bandwidth.
        let first = {
            let mut a = make_adaptation("audio", &[320_000]);
            a.id = Some("first".into());
            a
        };
        let second = {
            let mut a = make_adaptation("audio", &[320_000]);
            a.id = Some("second".into());
            a
        };
        let mut period = Period::default();
        period.adaptations = vec![first, second];

        let found = find_adaptation(&period, &MediaType::Audio).unwrap();
        assert_eq!(
            found.id.as_deref(),
            Some("first"),
            "document-order first should win on equal bandwidth"
        );
    }

    #[test]
    fn find_adaptation_detects_video_via_mime_type() {
        let mut a = AdaptationSet::default();
        a.mimeType = Some("video/mp4".into()); // no contentType set
        let mut period = Period::default();
        period.adaptations = vec![a];
        assert!(find_adaptation(&period, &MediaType::Video).is_some());
    }

    // ── extract_segment_timing ────────────────────────────────────────────────

    fn adaptation_with_template(dur: f64, ts: u64, start: u64) -> AdaptationSet {
        let mut a = AdaptationSet::default();
        a.SegmentTemplate = Some(SegmentTemplate {
            duration: Some(dur),
            timescale: Some(ts),
            startNumber: Some(start),
            ..Default::default()
        });
        a.representations = vec![Representation::default()];
        a
    }

    #[test]
    fn extract_segment_timing_basic() {
        // 2-second segments: duration=180000, timescale=90000
        let a = adaptation_with_template(180_000.0, 90_000, 1);
        let (start, dur_ms, total) = extract_segment_timing(
            &a,
            &Period::default(),
            Some(core::time::Duration::from_secs(60)),
        );
        assert_eq!(start, 1);
        assert!(
            (dur_ms - 2_000.0).abs() < 1.0,
            "expected 2000 ms, got {dur_ms}"
        );
        // 60 s / 2 s = 30 segments
        assert_eq!(total, Some(30));
    }

    #[test]
    fn extract_segment_timing_defaults_when_no_template() {
        // AdaptationSet with no SegmentTemplate at any level.
        let (start, dur_ms, total) =
            extract_segment_timing(&AdaptationSet::default(), &Period::default(), None);
        assert_eq!(start, 1);
        assert!((dur_ms - 2_000.0).abs() < 1.0);
        assert!(total.is_none());
    }

    #[test]
    fn extract_segment_timing_live_returns_none_total() {
        // Dynamic stream: no mediaPresentationDuration, no Period duration.
        let a = adaptation_with_template(96_000.0, 48_000, 1);
        let (_, _, total) = extract_segment_timing(&a, &Period::default(), None);
        assert!(
            total.is_none(),
            "live stream should have None total_segments"
        );
    }

    #[test]
    fn extract_segment_timing_infinite_repeat_returns_none_total() {
        let mut a = AdaptationSet::default();
        let mut tmpl = SegmentTemplate::default();
        tmpl.timescale = Some(1_000);
        tmpl.SegmentTimeline = Some(SegmentTimeline {
            segments: vec![S {
                t: Some(0),
                d: 2_000,
                r: Some(-1),
            }],
        });
        a.SegmentTemplate = Some(tmpl);
        a.representations = vec![Representation::default()];

        let (_, _, total) = extract_segment_timing(
            &a,
            &Period::default(),
            Some(core::time::Duration::from_secs(3600)),
        );
        assert!(
            total.is_none(),
            "infinite-repeat SegmentTimeline must return None for total_segments"
        );
    }

    #[test]
    fn extract_segment_timing_finite_timeline_counts_correctly() {
        // SegmentTimeline with 3 explicit entries (r=0 each → 1 segment per entry).
        let mut a = AdaptationSet::default();
        let mut tmpl = SegmentTemplate::default();
        tmpl.timescale = Some(1_000);
        tmpl.SegmentTimeline = Some(SegmentTimeline {
            segments: vec![
                S {
                    t: Some(0),
                    d: 2_000,
                    r: None,
                },
                S {
                    t: Some(2_000),
                    d: 2_000,
                    r: None,
                },
                S {
                    t: Some(4_000),
                    d: 2_000,
                    r: None,
                },
            ],
        });
        a.SegmentTemplate = Some(tmpl);
        a.representations = vec![Representation::default()];

        let (_, _, total) = extract_segment_timing(&a, &Period::default(), None);
        assert_eq!(total, Some(3));
    }

    // ── extract_representations ───────────────────────────────────────────────

    #[test]
    fn extract_representations_sorted_ascending() {
        let mut a = AdaptationSet::default();
        a.representations = vec![
            {
                let mut r = Representation::default();
                r.bandwidth = Some(3_000_000);
                r.id = Some("high".into());
                r
            },
            {
                let mut r = Representation::default();
                r.bandwidth = Some(500_000);
                r.id = Some("low".into());
                r
            },
            {
                let mut r = Representation::default();
                r.bandwidth = Some(1_500_000);
                r.id = Some("mid".into());
                r
            },
        ];
        let reps = extract_representations(&a);
        assert_eq!(reps.len(), 3);
        assert_eq!(reps[0].bandwidth, 500_000, "index 0 should be lowest");
        assert_eq!(reps[2].bandwidth, 3_000_000, "index 2 should be highest");
    }

    #[test]
    fn extract_representations_skips_missing_bandwidth() {
        let mut a = AdaptationSet::default();
        a.representations = vec![
            {
                let mut r = Representation::default();
                r.bandwidth = Some(1_000_000);
                r
            },
            Representation::default(), // no bandwidth
        ];
        let reps = extract_representations(&a);
        assert_eq!(
            reps.len(),
            1,
            "representation without bandwidth should be skipped"
        );
    }

    // ── adaptation_mime_type ──────────────────────────────────────────────────

    #[test]
    fn adaptation_mime_type_as_level_mime_with_codecs() {
        let mut a = AdaptationSet::default();
        a.mimeType = Some("video/mp4".into());
        a.codecs = Some("avc1.64001f".into());
        assert_eq!(
            adaptation_mime_type(&a),
            r#"video/mp4;codecs="avc1.64001f""#
        );
    }

    #[test]
    fn adaptation_mime_type_falls_back_to_repr_codecs() {
        let mut a = AdaptationSet::default();
        a.mimeType = Some("audio/mp4".into());
        // No AS-level codecs; first repr has codecs.
        let mut r = Representation::default();
        r.codecs = Some("mp4a.40.2".into());
        a.representations = vec![r];
        assert_eq!(adaptation_mime_type(&a), r#"audio/mp4;codecs="mp4a.40.2""#);
    }

    #[test]
    fn adaptation_mime_type_no_codecs() {
        let mut a = AdaptationSet::default();
        a.mimeType = Some("video/mp4".into());
        assert_eq!(adaptation_mime_type(&a), "video/mp4");
    }

    #[test]
    fn adaptation_mime_type_from_repr_mime() {
        // No AS-level mimeType; first repr carries it.
        let mut a = AdaptationSet::default();
        let mut r = Representation::default();
        r.mimeType = Some("video/webm".into());
        r.codecs = Some("vp9".into());
        a.representations = vec![r];
        assert_eq!(adaptation_mime_type(&a), r#"video/webm;codecs="vp9""#);
    }

    #[test]
    fn adaptation_mime_type_ultimate_fallback() {
        assert_eq!(
            adaptation_mime_type(&AdaptationSet::default()),
            "application/octet-stream"
        );
    }

    // ── merge_segment_lists ───────────────────────────────────────────────────

    #[test]
    fn merge_segment_lists_none_when_absent() {
        assert!(merge_segment_lists(None, &AdaptationSet::default(), &Period::default()).is_none());
    }

    #[test]
    fn merge_segment_lists_repr_urls_win() {
        let mut repr = Representation::default();
        repr.SegmentList = Some(SegmentList {
            duration: Some(2_000),
            timescale: Some(1_000),
            segment_urls: vec![
                SegmentURL {
                    media: Some("seg1.m4s".into()),
                    ..Default::default()
                },
                SegmentURL {
                    media: Some("seg2.m4s".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });

        let mut a = AdaptationSet::default();
        a.SegmentList = Some(SegmentList {
            segment_urls: vec![SegmentURL {
                media: Some("other.m4s".into()),
                ..Default::default()
            }],
            ..Default::default()
        });

        let merged = merge_segment_lists(Some(&repr), &a, &Period::default()).unwrap();
        assert_eq!(merged.segment_urls.len(), 2, "repr-level URLs should win");
        assert_eq!(merged.segment_urls[0].media.as_deref(), Some("seg1.m4s"));
    }

    #[test]
    fn merge_segment_lists_duration_inherited_from_adaptation() {
        let mut repr = Representation::default();
        repr.SegmentList = Some(SegmentList {
            segment_urls: vec![SegmentURL {
                media: Some("s.m4s".into()),
                ..Default::default()
            }],
            ..Default::default()
        });

        let mut a = AdaptationSet::default();
        a.SegmentList = Some(SegmentList {
            duration: Some(4_000),
            timescale: Some(1_000),
            ..Default::default()
        });

        let merged = merge_segment_lists(Some(&repr), &a, &Period::default()).unwrap();
        assert_eq!(merged.duration, Some(4_000));
        assert_eq!(merged.timescale, Some(1_000));
    }

    // ── extract_segment_list_timing ───────────────────────────────────────────

    #[test]
    fn extract_segment_list_timing_basic() {
        let mut a = AdaptationSet::default();
        a.SegmentList = Some(SegmentList {
            duration: Some(2_000),
            timescale: Some(1_000),
            startNumber: Some(1),
            segment_urls: vec![
                SegmentURL {
                    media: Some("seg1.m4s".into()),
                    ..Default::default()
                },
                SegmentURL {
                    media: Some("seg2.m4s".into()),
                    ..Default::default()
                },
                SegmentURL {
                    media: Some("seg3.m4s".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let mut r = Representation::default();
        r.bandwidth = Some(1_000_000);
        a.representations = vec![r];

        let (start, dur_ms, total) = extract_segment_list_timing(&a, &Period::default());
        // startNumber is not propagated through merge_segment_lists; default is 0.
        assert_eq!(start, 0);
        assert!((dur_ms - 2_000.0).abs() < 1.0);
        assert_eq!(total, Some(3));
    }

    #[test]
    fn extract_segment_list_timing_defaults_when_absent() {
        let (start, dur_ms, total) =
            extract_segment_list_timing(&AdaptationSet::default(), &Period::default());
        assert_eq!(start, 0);
        assert!((dur_ms - 2_000.0).abs() < 1.0);
        assert!(total.is_none());
    }

    // ── find_segment_base ─────────────────────────────────────────────────────

    #[test]
    fn find_segment_base_repr_wins() {
        let mut repr = Representation::default();
        repr.SegmentBase = Some(SegmentBase {
            indexRange: Some("100-200".into()),
            ..Default::default()
        });
        let mut a = AdaptationSet::default();
        a.SegmentBase = Some(SegmentBase {
            indexRange: Some("0-99".into()),
            ..Default::default()
        });

        let period = Period::default();
        let sb = find_segment_base(Some(&repr), &a, &period).unwrap();
        assert_eq!(sb.indexRange.as_deref(), Some("100-200"));
    }

    #[test]
    fn find_segment_base_falls_back_to_adaptation() {
        let mut a = AdaptationSet::default();
        a.SegmentBase = Some(SegmentBase {
            indexRange: Some("0-199".into()),
            ..Default::default()
        });
        let period = Period::default();
        let sb = find_segment_base(None, &a, &period).unwrap();
        assert_eq!(sb.indexRange.as_deref(), Some("0-199"));
    }

    #[test]
    fn find_segment_base_none_when_absent() {
        assert!(find_segment_base(None, &AdaptationSet::default(), &Period::default()).is_none());
    }

    // ── segment_base_media_path ───────────────────────────────────────────────

    #[test]
    fn segment_base_media_path_from_repr() {
        let mut repr = Representation::default();
        repr.BaseURL = vec![BaseURL {
            base: "video.mp4".into(),
            ..Default::default()
        }];
        let path = segment_base_media_path(Some(&repr), &AdaptationSet::default());
        assert_eq!(path, "video.mp4");
    }

    #[test]
    fn segment_base_media_path_from_adaptation() {
        let mut a = AdaptationSet::default();
        a.BaseURL = vec![BaseURL {
            base: "audio.mp4".into(),
            ..Default::default()
        }];
        let path = segment_base_media_path(None, &a);
        assert_eq!(path, "audio.mp4");
    }

    #[test]
    fn segment_base_media_path_empty_when_absent() {
        let path = segment_base_media_path(None, &AdaptationSet::default());
        assert!(path.is_empty());
    }
}
