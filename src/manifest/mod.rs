pub mod mpd_types;
pub mod parser;

use alloc::sync::Arc;

#[cfg(not(feature = "std"))]
use alloc::string::String;

use crate::Url;

use crate::{
    error::DashError,
    fetch::{FetchRequest, RequestType, SegmentFetcher},
};

// Re-export the vendored types so callers don't need to reach into sub-modules.
pub use mpd_types::MPD;
pub use parser::{parse, MpdParseError};

/// Loads and parses MPEG-DASH manifests from a URL.
///
/// Mirrors the responsibilities of `ManifestLoader.js` in dash.js:
/// - Fetches the raw XML text via the provided [`SegmentFetcher`].
/// - Handles redirect tracking (the `final_url` from the response is used
///   as the manifest's base URI for relative segment URL resolution).
/// - Delegates XML parsing to [`parse`].
///
/// Manifest refreshing (live streams) is the responsibility of the caller
/// or a higher-level `ManifestUpdater` component.
pub struct ManifestLoader<F: SegmentFetcher> {
    fetcher: Arc<F>,
}

impl<F: SegmentFetcher> ManifestLoader<F> {
    /// Create a new loader backed by the given fetcher.
    pub fn new(fetcher: Arc<F>) -> Self {
        Self { fetcher }
    }

    /// Fetch and parse the MPD manifest at `url`.
    ///
    /// On success returns the parsed [`MPD`] document together with the
    /// resolved base URL (the final URL after any redirects, used for
    /// resolving relative segment URLs later).
    pub async fn load(&self, url: &Url) -> Result<(MPD, Url), DashError> {
        let request = FetchRequest::new(url.clone(), RequestType::Manifest);
        let response = self.fetcher.fetch(request).await?;

        let base_url = response.final_url.clone();

        let text = String::from_utf8_lossy(&response.data);

        let mpd = parse(&text)?;

        Ok((mpd, base_url))
    }
}
