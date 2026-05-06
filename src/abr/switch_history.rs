use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::abr::{
    metrics::AbrMetrics,
    rule::{AbrRule, RepresentationInfo},
};

/// Minimum time between consecutive upward quality switches.
const MIN_SWITCH_INTERVAL: Duration = Duration::from_secs(8);
/// Number of recent switches to remember.
const HISTORY_CAPACITY: usize = 4;

/// Prevents rapid quality oscillation (thrashing) by requiring a minimum
/// interval between upward quality switches.
///
/// Mirrors `SwitchHistoryRule.js` from dash.js.
pub struct SwitchHistoryRule {
    /// Minimum wall-clock time between up-switches.
    pub min_switch_interval: Duration,
    /// Timestamps of recent quality switches (newest first).
    recent_switches: VecDeque<Instant>,
}

impl Default for SwitchHistoryRule {
    fn default() -> Self {
        Self {
            min_switch_interval: MIN_SWITCH_INTERVAL,
            recent_switches: VecDeque::with_capacity(HISTORY_CAPACITY),
        }
    }
}

impl SwitchHistoryRule {
    /// Record that a switch just occurred. Call this whenever `DashSession`
    /// actually changes quality.
    pub fn record_switch(&mut self) {
        let now = Instant::now();
        self.recent_switches.push_front(now);
        if self.recent_switches.len() > HISTORY_CAPACITY {
            self.recent_switches.pop_back();
        }
    }
}

impl AbrRule for SwitchHistoryRule {
    fn name(&self) -> &str {
        "SwitchHistoryRule"
    }

    fn max_index(
        &self,
        representations: &[RepresentationInfo],
        metrics: &AbrMetrics,
    ) -> Option<usize> {
        if representations.is_empty() {
            return None;
        }

        let current = metrics.current_index.unwrap_or(0);

        // Check whether the most recent switch was too recent.
        if let Some(last_switch) = self.recent_switches.front() {
            if last_switch.elapsed() < self.min_switch_interval {
                // Clamp to current quality to avoid further up-switching.
                tracing::debug!(
                    rule = self.name(),
                    elapsed_ms = last_switch.elapsed().as_millis(),
                    min_interval_ms = self.min_switch_interval.as_millis(),
                    current_index = current,
                    "switch too recent — clamping to current quality"
                );
                return Some(current);
            }
        }

        None
    }
}
