/// A snapshot of playback and network conditions supplied by the caller.
///
/// The caller is responsible for measuring these values and passing them in
/// before each quality decision. This mirrors the metrics that dash.js
/// collects internally in its `DashMetrics` and `ThroughputHistory` modules.
#[derive(Debug, Clone)]
pub struct AbrMetrics {
    /// Estimated available download bandwidth, in bits per second.
    /// Measured over recent segment downloads.
    pub throughput_bps: f64,

    /// Current playback buffer depth, in milliseconds.
    /// Higher values mean more buffer ahead of the current playback position.
    pub buffer_level_ms: f64,

    /// Round-trip latency for the last segment request, in milliseconds.
    pub latency_ms: f64,

    /// The index into the representation list that is currently playing.
    /// `None` if playback has not yet started.
    pub current_index: Option<usize>,
}

impl AbrMetrics {
    pub fn new(throughput_bps: f64, buffer_level_ms: f64) -> Self {
        Self {
            throughput_bps,
            buffer_level_ms,
            latency_ms: 0.0,
            current_index: None,
        }
    }
}
