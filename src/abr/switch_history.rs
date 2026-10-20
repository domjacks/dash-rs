use core::time::Duration;

#[cfg(not(feature = "std"))]
use alloc::collections::VecDeque;
#[cfg(feature = "std")]
use std::collections::VecDeque;

use crate::abr::{
    decision::AbrDecisionReason,
    metrics::AbrMetrics,
    rule::{AbrRule, RepresentationInfo},
};

/// Minimum time between consecutive upward quality switches.
pub(crate) const MIN_SWITCH_INTERVAL: Duration = Duration::from_secs(8);
/// Number of recent switches to remember.
const HISTORY_CAPACITY: usize = 4;

/// Prevents rapid quality oscillation (thrashing) by requiring a minimum
/// interval between upward quality switches.
///
/// Mirrors `SwitchHistoryRule.js` from dash.js.
pub struct SwitchHistoryRule {
    /// Minimum wall-clock time between up-switches.
    pub min_switch_interval: Duration,
    /// Timestamps (in ms, sourced from [`crate::clock::Clock::now_ms`]) of
    /// recent quality switches, newest first.
    recent_switches: VecDeque<f64>,
}

impl Default for SwitchHistoryRule {
    fn default() -> Self {
        Self::with_min_switch_interval(MIN_SWITCH_INTERVAL)
    }
}

impl SwitchHistoryRule {
    /// Create a rule with a custom minimum up-switch interval.
    pub fn with_min_switch_interval(min_switch_interval: Duration) -> Self {
        Self {
            min_switch_interval,
            recent_switches: VecDeque::with_capacity(HISTORY_CAPACITY),
        }
    }

    /// Record that a switch just occurred.
    ///
    /// `now_ms` must be sourced from [`crate::clock::Clock::now_ms`] — the
    /// same clock used to populate [`AbrMetrics::now_ms`].  Passing a value
    /// from any other source will corrupt elapsed-time calculations.
    pub fn record_switch(&mut self, now_ms: f64) {
        self.recent_switches.push_front(now_ms);
        if self.recent_switches.len() > HISTORY_CAPACITY {
            self.recent_switches.pop_back();
        }
    }
}

impl AbrRule for SwitchHistoryRule {
    fn name(&self) -> &str {
        "SwitchHistoryRule"
    }

    fn reason(&self) -> AbrDecisionReason {
        AbrDecisionReason::SwitchHistoryThrottle
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

        if let Some(&last_switch_ms) = self.recent_switches.front() {
            let elapsed_ms = metrics.now_ms - last_switch_ms;
            let min_interval_ms = self.min_switch_interval.as_secs_f64() * 1_000.0;
            if elapsed_ms < min_interval_ms {
                return Some(current);
            }
        }

        None
    }
}
