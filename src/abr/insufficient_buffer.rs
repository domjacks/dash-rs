use crate::abr::{
    metrics::AbrMetrics,
    rule::{AbrRule, RepresentationInfo},
};

/// Buffer level below which this rule forces the lowest quality, in ms.
const CRITICAL_BUFFER_THRESHOLD_MS: f64 = 2_000.0;

/// Emergency rule: if the buffer drops below a critical threshold, always
/// select the lowest-quality representation.
///
/// Mirrors `InsufficientBufferRule.js` from dash.js.
pub struct InsufficientBufferRule {
    /// Buffer level (ms) at or below which the rule fires. Default: 2 000 ms.
    pub critical_threshold_ms: f64,
}

impl Default for InsufficientBufferRule {
    fn default() -> Self {
        Self {
            critical_threshold_ms: CRITICAL_BUFFER_THRESHOLD_MS,
        }
    }
}

impl AbrRule for InsufficientBufferRule {
    fn name(&self) -> &str {
        "InsufficientBufferRule"
    }

    fn max_index(
        &self,
        representations: &[RepresentationInfo],
        metrics: &AbrMetrics,
    ) -> Option<usize> {
        if representations.is_empty() {
            return None;
        }

        // Immediately after a seek the buffer is transiently empty; abstain
        // so that quality is retained rather than forced to the lowest index.
        if metrics.is_post_seek {
            return None;
        }

        if metrics.buffer_level_ms <= self.critical_threshold_ms {
            tracing::warn!(
                rule = self.name(),
                buffer_level_ms = metrics.buffer_level_ms,
                threshold_ms = self.critical_threshold_ms,
                "buffer critically low — forcing lowest quality"
            );
            return Some(0);
        }

        // No opinion; let other rules decide.
        None
    }
}
