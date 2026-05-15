use thiserror::Error;

#[cfg(not(feature = "std"))]
use alloc::{boxed::Box, string::String};

/// The top-level error type for dash-rs.
#[derive(Debug, Error)]
pub enum DashError {
    // --- Manifest errors ---
    #[error("manifest fetch failed: {0}")]
    ManifestFetch(#[from] crate::fetch::FetchError),

    #[error("manifest parse failed: {0}")]
    ManifestParse(#[from] crate::manifest::MpdParseError),

    #[error("manifest is empty or contains no periods")]
    EmptyManifest,

    // --- Media source errors ---
    #[error("media source is not open (ready state: {state:?})")]
    MediaSourceNotOpen {
        state: crate::media_source::ReadyState,
    },

    #[error("source buffer not found for mime type: {mime_type}")]
    SourceBufferNotFound { mime_type: String },

    #[error("source buffer is currently updating")]
    SourceBufferUpdating,

    #[error("append failed: {0}")]
    AppendFailed(String),

    #[error("remove failed: {0}")]
    RemoveFailed(String),

    // --- Session errors ---
    #[error("no representations available for media type: {media_type}")]
    NoRepresentations { media_type: String },

    #[error("segment URL could not be resolved")]
    SegmentUrlUnresolvable,

    #[error("invalid time range: start={start} end={end}")]
    InvalidTimeRange { start: f64, end: f64 },

    /// All segments for this track have been fetched.
    ///
    /// Returned by [`crate::session::DashSession::fetch_next_segment`] once the
    /// segment number advances past the total derived from
    /// `MPD@mediaPresentationDuration`.  The caller should call
    /// [`crate::session::DashSession::end_of_stream`] and stop the fetch loop.
    #[error("end of stream: no more segments for {media_type}")]
    EndOfStream { media_type: String },

    /// Every `BaseURL` candidate for a segment was tried and all failed.
    ///
    /// The last underlying error is included for diagnostics.
    #[error("all CDN base URLs failed for segment (last error: {last_error})")]
    AllBaseUrlsFailed { last_error: String },

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
