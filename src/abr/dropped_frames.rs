//! [`DroppedFramesRule`] — forces lowest quality when the renderer is dropping
//! too many frames, indicating the current bitrate exceeds decoding capacity.

use crate::abr::{
    decision::AbrDecisionReason,
    metrics::AbrMetrics,
    rule::{AbrRule, RepresentationInfo},
};

/// Drop ratio above which the rule forces the lowest quality level.
///
/// Mirrors the `droppedFramesThreshold` constant (`0.1`) used by the
/// equivalent rule in dash.js.
const DROP_THRESHOLD: f64 = 0.10;

/// Forces the lowest quality representation when the decoder drop ratio
/// exceeds [`DROP_THRESHOLD`].
///
/// The caller must populate [`AbrMetrics::dropped_frames`] and
/// [`AbrMetrics::total_decoded_frames`] from renderer statistics (e.g.
/// GStreamer `GstVideoDecoder` dropped-frame counters) so this rule has data
/// to act on.  When either field is `None` the rule abstains.
///
/// Mirrors `DroppedFramesRule.js` in dash.js.
#[derive(Debug, Default)]
pub struct DroppedFramesRule;

impl AbrRule for DroppedFramesRule {
    fn name(&self) -> &str {
        "DroppedFramesRule"
    }

    fn reason(&self) -> AbrDecisionReason {
        AbrDecisionReason::DroppedFrames
    }

    fn max_index(
        &self,
        representations: &[RepresentationInfo],
        metrics: &AbrMetrics,
    ) -> Option<usize> {
        if representations.is_empty() {
            return None;
        }

        let (dropped, total) = match (metrics.dropped_frames, metrics.total_decoded_frames) {
            (Some(d), Some(t)) if t > 0 => (d as f64, t as f64),
            _ => return None,
        };

        let drop_ratio = dropped / total;
        if drop_ratio > DROP_THRESHOLD {
            tracing::debug!(
                drop_ratio,
                threshold = DROP_THRESHOLD,
                "DroppedFramesRule — forcing lowest quality"
            );
            Some(0)
        } else {
            None
        }
    }
}
