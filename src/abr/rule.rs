use crate::abr::{decision::AbrDecisionReason, metrics::AbrMetrics};

#[cfg(not(feature = "std"))]
use alloc::string::String;

/// A minimal representation of a DASH `Representation` for ABR decisions.
///
/// The caller constructs these from the parsed `dash_mpd::Representation`
/// values so that ABR rules remain decoupled from the MPD parser types.
#[derive(Debug, Clone)]
pub struct RepresentationInfo {
    /// The representation `id` attribute from the MPD.
    pub id: String,
    /// Bitrate in bits per second (`@bandwidth` from the MPD).
    pub bandwidth: u64,
    /// Optional display width in pixels.
    pub width: Option<u32>,
    /// Optional display height in pixels.
    pub height: Option<u32>,
}

/// Trait implemented by every ABR rule.
///
/// Each rule receives the ordered list of representations (ascending bitrate)
/// and the current playback metrics, and returns the *maximum* quality index
/// it is comfortable allowing. The [`crate::abr::AbrController`] takes the
/// minimum across all rules.
pub trait AbrRule {
    /// Returns the name of this rule, used in logging.
    fn name(&self) -> &str;

    /// Returns the decision reason to report when this rule constrains
    /// selection.
    fn reason(&self) -> AbrDecisionReason {
        AbrDecisionReason::ThroughputBased
    }

    /// Returns the highest representation index this rule permits, or `None`
    /// if the rule has no opinion (abstain).
    fn max_index(
        &self,
        representations: &[RepresentationInfo],
        metrics: &AbrMetrics,
    ) -> Option<usize>;
}
