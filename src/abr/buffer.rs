use crate::abr::{metrics::AbrMetrics, rule::{AbrRule, RepresentationInfo}};

/// Target buffer level in ms: the level we aim to maintain.
const TARGET_BUFFER_MS: f64 = 12_000.0;
/// Maximum buffer level in ms: above this there is no reason to download more.
const MAX_BUFFER_MS: f64 = 30_000.0;
/// Lyapunov parameter. Higher values weight throughput more; lower values
/// weight buffer occupancy more. Typical range: 5–20.
const GAMA: f64 = 5.0;

/// Buffer-occupancy-based ABR rule, inspired by the BOLA (Buffer Occupancy
/// based Lyapunov Algorithm) approach used in `BolaRule.js` from dash.js.
///
/// The rule maps current buffer occupancy to a utility score for each
/// representation and selects the one that maximises the Lyapunov objective:
///
/// ```text
/// V * utility(r) - buffer_level
/// ```
///
/// where `utility(r) = log2(bandwidth_r / bandwidth_0)` (log utility, 0 for
/// the lowest bitrate representation).
pub struct BufferOccupancyRule {
    /// Buffer level (ms) below which we force a down-switch.
    pub target_buffer_ms: f64,
    /// Buffer level (ms) above which we are free to use the highest quality.
    pub max_buffer_ms: f64,
    /// Lyapunov tuning parameter V.
    pub gama: f64,
}

impl Default for BufferOccupancyRule {
    fn default() -> Self {
        Self {
            target_buffer_ms: TARGET_BUFFER_MS,
            max_buffer_ms: MAX_BUFFER_MS,
            gama: GAMA,
        }
    }
}

impl AbrRule for BufferOccupancyRule {
    fn name(&self) -> &str {
        "BufferOccupancyRule"
    }

    fn max_index(
        &self,
        representations: &[RepresentationInfo],
        metrics: &AbrMetrics,
    ) -> Option<usize> {
        if representations.is_empty() {
            return None;
        }

        // If we have not built up any buffer yet, abstain so ThroughputRule
        // makes the first few decisions.
        if metrics.buffer_level_ms < 1.0 {
            return None;
        }

        let min_bandwidth = representations[0].bandwidth as f64;
        let buffer_norm =
            (metrics.buffer_level_ms / self.max_buffer_ms).clamp(0.0, 1.0);

        let mut best_index = 0;
        let mut best_score = f64::NEG_INFINITY;

        for (i, rep) in representations.iter().enumerate() {
            // Log utility relative to lowest representation (utility[0] == 0).
            let utility = if rep.bandwidth as f64 <= min_bandwidth {
                0.0
            } else {
                (rep.bandwidth as f64 / min_bandwidth).log2()
            };

            // BOLA objective: V * (utility + 1) - buffer_norm * (i + 1) as a
            // simplified variant that rewards filling the buffer.
            let score = self.gama * (utility + 1.0) - buffer_norm * (i as f64 + 1.0);

            if score > best_score {
                best_score = score;
                best_index = i;
            }
        }

        tracing::debug!(
            rule = self.name(),
            buffer_level_ms = metrics.buffer_level_ms,
            buffer_norm,
            selected_index = best_index,
            "buffer occupancy rule decision"
        );

        Some(best_index)
    }
}
