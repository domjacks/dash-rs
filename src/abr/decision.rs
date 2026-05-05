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
