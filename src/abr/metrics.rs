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

    /// Number of video frames dropped by the decoder since the last metrics
    /// snapshot. Set by the caller from renderer statistics (e.g.
    /// `GstVideoDecoder` dropped-frame counters).  `None` if unavailable.
    pub dropped_frames: Option<u64>,

    /// Total video frames decoded (presented + dropped) since the last metrics
    /// snapshot. Used with [`Self::dropped_frames`] to compute the drop ratio.
    /// `None` if unavailable.
    pub total_decoded_frames: Option<u64>,

    /// Ratio of the effective in-flight download speed to the current
    /// representation bitrate.  Set by the session when it detects that an
    /// in-flight segment is downloading more slowly than its playback deadline
    /// requires.  A value below `0.5` triggers [`AbandonRequestsRule`].
    ///
    /// [`AbandonRequestsRule`]: crate::abr::abandon_requests::AbandonRequestsRule
    pub effective_download_ratio: Option<f64>,

    /// `true` on the first [`DashSession::fetch_next_segment`] call after a
    /// seek.  When set, buffer-occupancy-based rules ([`InsufficientBufferRule`]
    /// and [`BufferOccupancyRule`]) abstain so that quality selection is driven
    /// purely by throughput rather than the transiently-empty post-seek buffer.
    ///
    /// Set automatically by [`DashSession::current_metrics`] from
    /// [`TrackState::post_seek`]; callers do not need to set this manually.
    pub is_post_seek: bool,
}

impl AbrMetrics {
    pub fn new(throughput_bps: f64, buffer_level_ms: f64) -> Self {
        Self {
            throughput_bps,
            buffer_level_ms,
            latency_ms: 0.0,
            current_index: None,
            dropped_frames: None,
            total_decoded_frames: None,
            effective_download_ratio: None,
            is_post_seek: false,
        }
    }
}
