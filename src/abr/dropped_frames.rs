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
pub(crate) const DROP_THRESHOLD: f64 = 0.10;

/// Forces the lowest quality representation when the decoder drop ratio
/// exceeds [`Self::threshold`].
///
/// The caller must populate [`AbrMetrics::dropped_frames`] and
/// [`AbrMetrics::total_decoded_frames`] from renderer statistics (e.g.
/// GStreamer `GstVideoDecoder` dropped-frame counters) so this rule has data
/// to act on.  When either field is `None` the rule abstains.
///
/// Mirrors `DroppedFramesRule.js` in dash.js.
#[derive(Debug)]
pub struct DroppedFramesRule {
    /// Drop ratio above which the rule fires. Default: 0.10.
    pub threshold: f64,
}

impl Default for DroppedFramesRule {
    fn default() -> Self {
        Self {
            threshold: DROP_THRESHOLD,
        }
    }
}

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
        if drop_ratio > self.threshold {
            Some(0)
        } else {
            None
        }
    }
}
