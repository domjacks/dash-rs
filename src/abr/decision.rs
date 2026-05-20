/// Why a particular quality index was chosen.
#[derive(Debug, Clone, PartialEq)]
pub enum AbrDecisionReason {
    /// Initial quality selection before any playback has occurred.
    Initial,
    /// Throughput-based selection: bandwidth comfortably supports this level.
    ThroughputBased,
    /// Buffer-occupancy-based selection (BOLA algorithm).
    BufferOccupancyBased,
    /// Emergency down-switch because the buffer is critically low.
    InsufficientBuffer,
    /// Up-switch was suppressed to avoid quality thrashing.
    SwitchHistoryThrottle,
    /// The quality was clamped to the lowest level due to a rule override.
    Override,
    /// Down-switch forced because the decoder is dropping too many frames.
    DroppedFrames,
    /// Down-switch forced because the in-flight segment download was abandoned
    /// for being too slow relative to the representation bitrate.
    RequestAbandoned,
}

impl core::fmt::Display for AbrDecisionReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::Initial => "initial",
            Self::ThroughputBased => "throughput",
            Self::BufferOccupancyBased => "buffer-occupancy",
            Self::InsufficientBuffer => "insufficient-buffer",
            Self::SwitchHistoryThrottle => "switch-history-throttle",
            Self::Override => "override",
            Self::DroppedFrames => "dropped-frames",
            Self::RequestAbandoned => "request-abandoned",
        };
        f.write_str(s)
    }
}

/// The output from [`crate::abr::AbrController`]: which representation index
/// to use next, and why.
#[derive(Debug, Clone)]
pub struct QualityDecision {
    /// Index into the representation list (sorted ascending by bitrate).
    pub index: usize,
    /// The reason this index was chosen.
    pub reason: AbrDecisionReason,
}

impl QualityDecision {
    pub fn new(index: usize, reason: AbrDecisionReason) -> Self {
        Self { index, reason }
    }
}

impl core::fmt::Display for QualityDecision {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "index={} reason={}", self.index, self.reason)
    }
}
