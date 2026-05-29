//! [`AbandonRequestsRule`] — down-switches when an in-flight segment fetch is
//! progressing too slowly to finish before its playback deadline.

use crate::abr::{
    decision::AbrDecisionReason,
    metrics::AbrMetrics,
    rule::{AbrRule, RepresentationInfo},
};

/// Ratio below which a slow in-flight download triggers a quality drop.
///
/// Mirrors the `abandonMultiplier` constant (`0.5`) from dash.js's
/// `AbandonRequestsRule`.
const ABANDON_MULTIPLIER: f64 = 0.5;

/// Forces the lowest quality representation when
/// [`AbrMetrics::effective_download_ratio`] indicates the in-flight segment
/// is downloading too slowly.
///
/// The *effective download ratio* is defined as:
///
/// ```text
/// ratio = (bytes_received_so_far × 8) / elapsed_ms
///       / representation_bandwidth_bps
/// ```
///
/// The session sets this field via [`AbrMetrics::effective_download_ratio`]
/// when a timeout fires during a segment fetch.  If the field is `None` (the
/// common case when no slow download is detected) the rule abstains.
///
/// Mirrors `AbandonRequestsRule.js` in dash.js.
#[derive(Debug, Default)]
pub struct AbandonRequestsRule;

impl AbrRule for AbandonRequestsRule {
    fn name(&self) -> &str {
        "AbandonRequestsRule"
    }

    fn reason(&self) -> AbrDecisionReason {
        AbrDecisionReason::RequestAbandoned
    }

    fn max_index(
        &self,
        representations: &[RepresentationInfo],
        metrics: &AbrMetrics,
    ) -> Option<usize> {
        if representations.is_empty() {
            return None;
        }

        let ratio = metrics.effective_download_ratio?;
        if ratio < ABANDON_MULTIPLIER {
            Some(0)
        } else {
            None
        }
    }
}
