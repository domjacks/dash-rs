//! MPD manifest helpers.
//!
//! Extracts and merges the `SegmentTemplate` hierarchy, enumerates
//! `Representation`s, and derives per-track timing parameters from the MPD.
//!
//! Mirrors `MpdHelper.js`, `DashManifestModel.js`, and parts of
//! `TemplateSegmentsGetter.js` in dash.js.

use dash_mpd::{AdaptationSet, Period, Representation, SegmentTemplate};

use crate::abr::RepresentationInfo;
use super::MediaType;
use super::timeline::count_timeline_segments;

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
        if merged.Initialization.is_none() {
            merged.Initialization.clone_from(&tmpl.Initialization);
        }
        if merged.SegmentTimeline.is_none() {
            merged.SegmentTimeline.clone_from(&tmpl.SegmentTimeline);
        }
    }

    Some(merged)
}

/// Return a reference to the first adaptation set matching `media_type`
/// within `period`.
pub(super) fn find_adaptation<'a>(
    period: &'a Period,
    media_type: &MediaType,
) -> Option<&'a AdaptationSet> {
    period.adaptations.iter().find(|a| match media_type {
        MediaType::Video => dash_mpd::is_video_adaptation(a),
        MediaType::Audio => dash_mpd::is_audio_adaptation(a),
    })
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
    presentation_duration: Option<std::time::Duration>,
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
        _ => {
            t.SegmentTimeline
                .as_ref()
                .and_then(|tl| tl.segments.first())
                .map(|s| {
                    let ts = t.timescale.unwrap_or(1) as f64;
                    s.d as f64 / ts * 1_000.0
                })
                .unwrap_or(2_000.0)
        }
    };

    // For SegmentTimeline streams derive total_segments from the timeline count.
    // For live streams (no mediaPresentationDuration) this will be None.
    let total_segments = if let Some(tl) = &t.SegmentTimeline {
        let count = count_timeline_segments(&tl.segments);
        if count > 0 { Some(count) } else { None }
    } else {
        let effective_duration = presentation_duration.or(period.duration);
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
pub(super) fn adaptation_mime_type(adaptation: &AdaptationSet) -> String {
    if let Some(mime) = &adaptation.mimeType {
        if let Some(rep) = adaptation.representations.first() {
            if let Some(codecs) = &rep.codecs {
                return format!("{};codecs=\"{}\"", mime, codecs);
            }
        }
        return mime.clone();
    }

    if let Some(rep) = adaptation.representations.first() {
        if let Some(mime) = &rep.mimeType {
            if let Some(codecs) = &rep.codecs {
                return format!("{};codecs=\"{}\"", mime, codecs);
            }
            return mime.clone();
        }
    }

    "application/octet-stream".to_owned()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::merge_segment_templates;
    use dash_mpd::{AdaptationSet, Period, Representation, SegmentTemplate};

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
