use thiserror::Error;

#[cfg(not(feature = "std"))]
use alloc::string::String;

/// The top-level error type for dash-rs.
#[derive(Debug, Error, PartialEq)]
pub enum DashError {
    // --- Manifest errors ---
    #[error("manifest fetch failed: {0}")]
    ManifestFetch(#[from] crate::fetch::FetchError),

    #[error("manifest parse failed: {0}")]
    ManifestParse(#[from] crate::manifest::MpdParseError),

    #[error("manifest is empty or contains no periods")]
    EmptyManifest,

    // --- Media buffer errors ---
    #[error("append failed: {message}")]
    AppendFailed { message: String },

    #[error("remove failed: {message}")]
    RemoveFailed { message: String },

    // --- Session errors ---
    #[error("no representations available for media type: {media_type}")]
    NoRepresentations { media_type: String },

    #[error("segment URL could not be resolved")]
    SegmentUrlUnresolvable,

    #[error("invalid time range: start={start} end={end}")]
    InvalidTimeRange { start: f64, end: f64 },

    /// Every `BaseURL` candidate for a segment was tried and all failed.
    ///
    /// The last underlying error is included for diagnostics.
    #[error("all CDN base URLs failed for segment (last error: {last_error})")]
    AllBaseUrlsFailed { last_error: String },

    /// Internal sentinel used by `fetch_next_segment_inner` to signal
    /// period-boundary end-of-stream.  Converted to
    /// [`crate::session::FetchOutcome::EndOfStream`] at the public
    /// `fetch_next_segment` boundary and should never escape to callers.
    #[doc(hidden)]
    #[error("end of stream: no more segments for {media_type}")]
    EndOfStream { media_type: String },

    /// A segment fetch was abandoned because it was taking too long.
    ///
    /// Triggered when [`crate::session::DashSession::set_abandon_timeout_ms`]
    /// is set and the fetch does not complete within the configured duration.
    /// The caller should lower quality and retry.
    #[error("segment fetch abandoned (too slow) for {media_type}")]
    SegmentAbandoned { media_type: String },

    /// A live segment returned HTTP 404, indicating it has not yet been
    /// generated on the CDN.
    ///
    /// [`crate::session::DashSession::fetch_next_segment`] handles this
    /// internally by waiting one segment duration before retrying.  If the
    /// segment is still unavailable after the retry, `AllBaseUrlsFailed` is
    /// returned instead.
    #[error("live segment not yet available for {media_type} (HTTP 404)")]
    SegmentNotYetAvailable { media_type: String },
}
