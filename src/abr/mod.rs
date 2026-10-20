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
use alloc::{boxed::Box, vec, vec::Vec};

use crate::abr::{
    abandon_requests::AbandonRequestsRule, buffer::BufferOccupancyRule,
    dropped_frames::DroppedFramesRule, insufficient_buffer::InsufficientBufferRule,
    switch_history::SwitchHistoryRule, throughput::ThroughputRule,
};
use crate::settings::AbrSettings;

/// Aggregates multiple [`AbrRule`]s and returns a single [`QualityDecision`].
///
/// Rules are evaluated in registration order. The final index is the minimum
/// (most conservative) across all rules that express an opinion. This mirrors
/// the rule pipeline in dash.js's `AbrController.js`.
pub struct AbrController {
    rules: Vec<Box<dyn AbrRule>>,
    /// Separate mutable handle to the switch-history rule so it can be notified
    /// when a switch actually occurs.
    switch_history: Option<SwitchHistoryRule>,
}

impl Default for AbrController {
    /// Creates a controller with the standard rule pipeline.
    fn default() -> Self {
        Self::from_settings(&AbrSettings::default())
    }
}

impl AbrController {
    /// Create the standard rule pipeline tuned by `settings`.
    pub fn from_settings(settings: &AbrSettings) -> Self {
        Self {
            rules: vec![
                Box::new(ThroughputRule::with_safety_factor(
                    settings.throughput_safety_factor,
                )),
                Box::new(BufferOccupancyRule {
                    target_buffer_ms: settings.bola_target_buffer_ms,
                    max_buffer_ms: settings.bola_max_buffer_ms,
                    gamma: settings.bola_gamma,
                }),
                Box::new(InsufficientBufferRule {
                    critical_threshold_ms: settings.insufficient_buffer_threshold_ms,
                }),
                Box::new(DroppedFramesRule {
                    threshold: settings.dropped_frames_threshold,
                }),
                Box::new(AbandonRequestsRule {
                    multiplier: settings.abandon_multiplier,
                }),
            ],
            switch_history: settings
                .min_switch_interval
                .map(SwitchHistoryRule::with_min_switch_interval),
        }
    }

    /// Create a controller with a custom set of rules and default switch
    /// history throttling.
    pub fn with_rules(rules: Vec<Box<dyn AbrRule>>) -> Self {
        Self::with_rules_and_switch_history(rules, Some(SwitchHistoryRule::default()))
    }

    /// Create a controller with custom rules and optional switch-history
    /// throttling.
    pub fn with_rules_and_switch_history(
        rules: Vec<Box<dyn AbrRule>>,
        switch_history: Option<SwitchHistoryRule>,
    ) -> Self {
        Self {
            rules,
            switch_history,
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

        if metrics.is_post_seek {
            let index = metrics
                .current_index
                .unwrap_or(0)
                .min(representations.len() - 1);
            return QualityDecision::new(index, AbrDecisionReason::ThroughputBased);
        }

        // Collect opinions from all stateless rules.
        let mut max_index = representations.len() - 1;
        let mut reason = AbrDecisionReason::ThroughputBased;

        // Check switch history first (it can clamp up-switches).
        if let Some(switch_history) = &self.switch_history {
            if let Some(idx) = switch_history.max_index(representations, metrics) {
                if idx < max_index {
                    max_index = idx;
                    reason = AbrDecisionReason::SwitchHistoryThrottle;
                }
            }
        }

        for rule in &self.rules {
            if let Some(idx) = rule.max_index(representations, metrics) {
                if idx < max_index {
                    max_index = idx;
                    reason = rule.reason();
                }
            }
        }

        QualityDecision::new(max_index, reason)
    }

    /// Notify the controller that a quality switch has occurred.
    ///
    /// `now_ms` must be sourced from [`crate::clock::Clock::now_ms`] — the
    /// same clock used to populate [`AbrMetrics::now_ms`] — so that elapsed
    /// time is computed correctly on all targets, including `no_std`.
    pub fn record_switch(&mut self, now_ms: f64) {
        if let Some(switch_history) = self.switch_history.as_mut() {
            switch_history.record_switch(now_ms);
        }
    }
}
