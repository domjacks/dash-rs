use thiserror::Error;

/// The top-level error type for dash-rs.
#[derive(Debug, Error)]
pub enum DashError {
    // --- Manifest errors ---
    #[error("manifest fetch failed: {0}")]
    ManifestFetch(#[from] crate::fetch::FetchError),

    #[error("manifest parse failed: {0}")]
    ManifestParse(#[from] dash_mpd::DashMpdError),

    #[error("manifest is empty or contains no periods")]
    EmptyManifest,

    // --- Media source errors ---
    #[error("media source is not open (ready state: {state:?})")]
    MediaSourceNotOpen { state: crate::media_source::ReadyState },

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
}
