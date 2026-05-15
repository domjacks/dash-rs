pub mod abandon_requests;
pub mod buffer;
pub mod decision;
pub mod dropped_frames;
pub mod insufficient_buffer;
pub mod metrics;
pub mod rule;
pub mod switch_history;
pub mod throughput;

pub use decision::{AbrDecisionReason, QualityDecision};
pub use metrics::AbrMetrics;
pub use rule::{AbrRule, RepresentationInfo};

#[cfg(not(feature = "std"))]
use alloc::{boxed::Box, vec::Vec};

use crate::abr::{
    abandon_requests::AbandonRequestsRule, buffer::BufferOccupancyRule,
    dropped_frames::DroppedFramesRule, insufficient_buffer::InsufficientBufferRule,
    switch_history::SwitchHistoryRule, throughput::ThroughputRule,
};

/// Aggregates multiple [`AbrRule`]s and returns a single [`QualityDecision`].
///
/// Rules are evaluated in registration order. The final index is the minimum
/// (most conservative) across all rules that express an opinion. This mirrors
/// the rule pipeline in dash.js's `AbrController.js`.
pub struct AbrController {
    rules: Vec<Box<dyn AbrRule>>,
    /// Separate mutable handle to the switch-history rule so it can be notified
    /// when a switch actually occurs.
    switch_history: SwitchHistoryRule,
}

impl Default for AbrController {
    /// Creates a controller with the standard rule pipeline.
    fn default() -> Self {
        Self {
            rules: vec![
                Box::new(ThroughputRule::default()),
                Box::new(BufferOccupancyRule::default()),
                Box::new(InsufficientBufferRule::default()),
                Box::new(DroppedFramesRule),
                Box::new(AbandonRequestsRule),
            ],
            switch_history: SwitchHistoryRule::default(),
        }
    }
}

impl AbrController {
    /// Create a controller with a custom set of rules.
    pub fn with_rules(rules: Vec<Box<dyn AbrRule>>) -> Self {
        Self {
            rules,
            switch_history: SwitchHistoryRule::default(),
        }
    }

    /// Compute the recommended [`QualityDecision`] given the current metrics
    /// and the available representations (must be sorted ascending by bitrate).
    pub fn select(
        &self,
        representations: &[RepresentationInfo],
        metrics: &AbrMetrics,
    ) -> QualityDecision {
        if representations.is_empty() {
            return QualityDecision::new(0, AbrDecisionReason::Initial);
        }

        // Collect opinions from all stateless rules.
        let mut max_index = representations.len() - 1;
        let mut reason = AbrDecisionReason::ThroughputBased;

        // Check switch history first (it can clamp up-switches).
        if let Some(idx) = self.switch_history.max_index(representations, metrics) {
            if idx < max_index {
                max_index = idx;
                reason = AbrDecisionReason::SwitchHistoryThrottle;
            }
        }

        for rule in &self.rules {
            if let Some(idx) = rule.max_index(representations, metrics) {
                if idx < max_index {
                    max_index = idx;
                    reason = match rule.name() {
                        "InsufficientBufferRule" => AbrDecisionReason::InsufficientBuffer,
                        "BufferOccupancyRule" => AbrDecisionReason::BufferOccupancyBased,
                        "DroppedFramesRule" => AbrDecisionReason::DroppedFrames,
                        "AbandonRequestsRule" => AbrDecisionReason::RequestAbandoned,
                        _ => AbrDecisionReason::ThroughputBased,
                    };
                }
            }
        }

        tracing::debug!(
            selected_index = max_index,
            reason = ?reason,
            "ABR controller decision"
        );

        QualityDecision::new(max_index, reason)
    }

    /// Notify the controller that a quality switch has occurred.
    /// Should be called by `DashSession` whenever it changes representations.
    pub fn record_switch(&mut self) {
        self.switch_history.record_switch();
    }
}
