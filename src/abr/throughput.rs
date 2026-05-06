use crate::abr::{
    metrics::AbrMetrics,
    rule::{AbrRule, RepresentationInfo},
};

/// The fraction of measured throughput that must exceed a representation's
/// bitrate before that representation is selected. A value of 0.9 means the
/// measured bandwidth must be at least 11% above the bitrate (safety margin).
const SAFETY_FACTOR: f64 = 0.9;

/// Selects the highest quality whose bitrate fits within the available
/// bandwidth, applying a configurable safety factor.
///
/// Mirrors `ThroughputRule.js` from dash.js.
pub struct ThroughputRule {
    /// Fraction of measured throughput to treat as usable. Default: 0.9.
    pub safety_factor: f64,
}

impl Default for ThroughputRule {
    fn default() -> Self {
        Self {
            safety_factor: SAFETY_FACTOR,
        }
    }
}

impl AbrRule for ThroughputRule {
    fn name(&self) -> &str {
        "ThroughputRule"
    }

    fn max_index(
        &self,
        representations: &[RepresentationInfo],
        metrics: &AbrMetrics,
    ) -> Option<usize> {
        if representations.is_empty() {
            return None;
        }

        let usable_bps = metrics.throughput_bps * self.safety_factor;

        // Walk representations from highest to lowest; pick the first that fits.
        let selected = representations
            .iter()
            .enumerate()
            .rev()
            .find(|(_, r)| (r.bandwidth as f64) <= usable_bps)
            .map(|(i, _)| i)
            .unwrap_or(0); // always fall back to the lowest quality

        tracing::debug!(
            rule = self.name(),
            throughput_bps = metrics.throughput_bps,
            usable_bps,
            selected_index = selected,
            selected_bandwidth = representations[selected].bandwidth,
            "throughput rule decision"
        );

        Some(selected)
    }
}
