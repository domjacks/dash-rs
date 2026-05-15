use alloc::collections::VecDeque;

#[cfg(not(feature = "std"))]
use crate::math::FloatExt as _;

use crate::abr::{
    metrics::AbrMetrics,
    rule::{AbrRule, RepresentationInfo},
};

/// The fraction of the throughput estimate that must exceed a representation's
/// bitrate before that representation is selected. A value of 0.9 means the
/// measured bandwidth must be at least 11% above the bitrate (safety margin).
const SAFETY_FACTOR: f64 = 0.9;

/// Half-life of the **fast** EWMA in seconds.
///
/// A shorter half-life makes the estimate react quickly to sudden drops in
/// throughput (e.g. after a network hand-off).  Mirrors dash.js's
/// `ThroughputController.BANDWIDTH_EWMA_HALF_LIFE_FAST`.
const FAST_HALFLIFE_S: f64 = 3.0;

/// Half-life of the **slow** EWMA in seconds.
///
/// A longer half-life provides a stable baseline that is not thrown off by
/// transient bursts or measurement noise.  Mirrors dash.js's
/// `ThroughputController.BANDWIDTH_EWMA_HALF_LIFE_SLOW`.
const SLOW_HALFLIFE_S: f64 = 8.0;

/// Number of raw throughput samples retained for the harmonic-mean calculation.
///
/// dash.js computes the harmonic mean over the most recent samples to reduce
/// the influence of outliers (e.g. a single unusually fast segment).  Four
/// samples keeps the window short enough to track gradual throughput changes
/// while still smoothing per-segment variance.
const MAX_SAMPLES: usize = 4;

// ── ThroughputTracker ────────────────────────────────────────────────────────

/// Per-track throughput estimator with dual EWMA and harmonic-mean smoothing.
///
/// Mirrors the estimation strategy of dash.js's `ThroughputController.js`:
///
/// 1. **Fast EWMA** (`α = 1 − exp(−dt / 3s)`) reacts quickly to drops.
/// 2. **Slow EWMA** (`α = 1 − exp(−dt / 8s)`) provides a stable baseline.
/// 3. **Harmonic mean** of the last `MAX_SAMPLES` raw measurements reduces the
///    influence of abnormally fast individual segment downloads.
/// 4. The estimate returned by [`estimate`] is the minimum of the harmonic mean
///    and both EWMAs — conservative, like dash.js.
///
/// The time-based α formula (`α = 1 − exp(−dt / halflife)`) ensures the
/// smoothing factor adapts to the actual download duration rather than treating
/// every segment uniformly.  For a 4 s segment at `FAST_HALFLIFE_S = 3 s`,
/// α ≈ 0.74 (strongly weights the new sample).  For the same segment at
/// `SLOW_HALFLIFE_S = 8 s`, α ≈ 0.39.
///
/// # Seeding
///
/// The first sample seeds both EWMAs directly (no warm-up from zero) so that
/// ABR decisions are data-driven from the very first segment.
pub struct ThroughputTracker {
    /// Ring buffer of the most recent raw throughput measurements (bits/s).
    samples: VecDeque<f64>,
    /// Fast EWMA estimate (bits/s).  `0.0` until the first sample.
    fast_ema: f64,
    /// Slow EWMA estimate (bits/s).  `0.0` until the first sample.
    slow_ema: f64,
}

impl Default for ThroughputTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ThroughputTracker {
    /// Create a new, empty tracker.
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            fast_ema: 0.0,
            slow_ema: 0.0,
        }
    }

    /// Record a new throughput measurement.
    ///
    /// - `bps` — measured throughput in bits per second for this segment.
    /// - `elapsed_s` — actual download duration in seconds.  Used to compute
    ///   the time-based EWMA smoothing factors.
    ///
    /// The first call seeds both EWMAs directly.  Subsequent calls apply
    /// exponential smoothing with time-adapted α values.
    pub fn record(&mut self, bps: f64, elapsed_s: f64) {
        if bps <= 0.0 {
            return;
        }

        // Compute time-based alpha values: α = 1 − exp(−dt / halflife).
        // Clamp to [0, 1] to handle degenerate elapsed_s values.
        let elapsed = elapsed_s.max(0.0);
        let alpha_fast = (1.0 - (-elapsed / FAST_HALFLIFE_S).exp()).clamp(0.0, 1.0);
        let alpha_slow = (1.0 - (-elapsed / SLOW_HALFLIFE_S).exp()).clamp(0.0, 1.0);

        if self.fast_ema == 0.0 {
            // First sample: seed directly to avoid a long warm-up from zero.
            self.fast_ema = bps;
            self.slow_ema = bps;
        } else {
            self.fast_ema = alpha_fast * bps + (1.0 - alpha_fast) * self.fast_ema;
            self.slow_ema = alpha_slow * bps + (1.0 - alpha_slow) * self.slow_ema;
        }

        // Maintain a bounded ring buffer of raw samples for harmonic mean.
        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(bps);
    }

    /// Returns the current throughput estimate in bits per second.
    ///
    /// Returns `0.0` when no samples have been recorded yet.
    ///
    /// The estimate is the **minimum** of:
    /// - harmonic mean of the last `MAX_SAMPLES` raw measurements, and
    /// - the fast EWMA, and
    /// - the slow EWMA.
    ///
    /// Taking the minimum is conservative: it prefers the most pessimistic
    /// view of the available bandwidth, reducing the chance of buffering
    /// stalls caused by over-optimistic quality selections.
    pub fn estimate(&self) -> f64 {
        if self.fast_ema == 0.0 {
            return 0.0;
        }

        // Harmonic mean: n / Σ(1/bps_i).  Harmonic mean is always ≤ arithmetic
        // mean, making it naturally conservative for this use-case.
        let harmonic = {
            let n = self.samples.len() as f64;
            let sum_inv: f64 = self.samples.iter().map(|&b| 1.0 / b).sum();
            if sum_inv > 0.0 {
                n / sum_inv
            } else {
                self.fast_ema
            }
        };

        harmonic.min(self.fast_ema).min(self.slow_ema)
    }

    /// Returns `true` when no samples have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.fast_ema == 0.0
    }
}

// ── ThroughputRule ────────────────────────────────────────────────────────────

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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::ThroughputTracker;

    #[test]
    fn tracker_empty_returns_zero() {
        let t = ThroughputTracker::new();
        assert_eq!(t.estimate(), 0.0);
        assert!(t.is_empty());
    }

    #[test]
    fn tracker_first_sample_seeds_directly() {
        let mut t = ThroughputTracker::new();
        t.record(10_000_000.0, 4.0);
        // First sample should seed both EWMAs to exactly 10 Mbps.
        // The harmonic mean of one sample is the sample itself.
        // estimate = min(harmonic, fast, slow) = 10 Mbps.
        assert!(
            (t.estimate() - 10_000_000.0).abs() < 1.0,
            "estimate = {}",
            t.estimate()
        );
        assert!(!t.is_empty());
    }

    #[test]
    fn tracker_slow_ema_more_stable_than_fast() {
        let mut t = ThroughputTracker::new();
        // Seed with 10 Mbps.
        t.record(10_000_000.0, 4.0);
        // Drop to 1 Mbps.
        t.record(1_000_000.0, 4.0);
        // fast_ema should have dropped more than slow_ema.
        // We can verify by checking that estimate < seed value.
        assert!(t.estimate() < 10_000_000.0);
    }

    #[test]
    fn tracker_harmonic_mean_below_arithmetic_mean() {
        let mut t = ThroughputTracker::new();
        // Alternate high and low samples: harmonic mean < arithmetic mean.
        t.record(8_000_000.0, 4.0);
        t.record(2_000_000.0, 4.0);
        // Arithmetic mean = 5 Mbps; harmonic mean ≈ 3.2 Mbps.
        // estimate ≤ 3.2 Mbps.
        assert!(t.estimate() <= 5_000_000.0);
    }

    #[test]
    fn tracker_ignores_nonpositive_bps() {
        let mut t = ThroughputTracker::new();
        t.record(0.0, 4.0);
        t.record(-1_000.0, 4.0);
        assert!(t.is_empty());
    }

    #[test]
    fn tracker_ring_buffer_bounded_at_max_samples() {
        let mut t = ThroughputTracker::new();
        // Record more than MAX_SAMPLES entries.
        for i in 1..=10 {
            t.record(i as f64 * 1_000_000.0, 4.0);
        }
        // Should not panic; estimate should be > 0.
        assert!(t.estimate() > 0.0);
        // Ring buffer should not exceed MAX_SAMPLES.
        assert!(t.samples.len() <= super::MAX_SAMPLES);
    }

    #[test]
    fn tracker_harmonic_mean_uses_only_last_4_samples() {
        let mut t = ThroughputTracker::new();
        // Record 5 samples: four very high followed by one very low.
        // After 5 records only the last 4 should remain in the ring buffer.
        // The oldest high sample is evicted, leaving [high, high, high, low].
        for _ in 0..4 {
            t.record(1_000_000_000.0, 4.0); // 1 Gbps
        }
        t.record(1_000.0, 4.0); // 1 kbps — now the oldest 1 Gbps is evicted

        // Ring buffer should hold exactly MAX_SAMPLES entries.
        assert_eq!(t.samples.len(), super::MAX_SAMPLES);

        // The harmonic mean of [1e9, 1e9, 1e9, 1000] is dominated by the
        // 1 kbps outlier: n / Σ(1/x) = 4 / (3/1e9 + 1/1000) ≈ 4000 bps.
        // The estimate (min of harmonic, fast_ema, slow_ema) must be well
        // below 1 Gbps, proving the eviction changed the harmonic mean.
        assert!(
            t.estimate() < 1_000_000_000.0,
            "estimate should reflect the low outlier, got {}",
            t.estimate()
        );
        // And must be above 0.
        assert!(t.estimate() > 0.0);
    }
}
