use async_trait::async_trait;
use bytes::Bytes;
use url::Url;

use crate::fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher};

/// A stub [`SegmentFetcher`] that logs every call and immediately returns
/// empty bytes without making any real network requests.
///
/// Useful for unit tests, offline tooling, and as a placeholder while
/// integrating the library into a larger system.
///
/// To simulate specific responses in tests, wrap this type or provide your
/// own implementation of [`SegmentFetcher`].
pub struct StubSegmentFetcher;

#[async_trait(?Send)]
impl SegmentFetcher for StubSegmentFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        tracing::debug!(
            url = %request.url,
            request_type = ?request.request_type,
            byte_range = ?request.byte_range,
            "StubSegmentFetcher: fetch (returning empty bytes)"
        );

        Ok(FetchResponse {
            final_url: request.url,
            data: Bytes::new(),
            transfer_duration_ms: 0,
            bytes_received: 0,
        })
    }

    async fn check_existence(&self, url: &Url) -> Result<bool, FetchError> {
        tracing::debug!(url = %url, "StubSegmentFetcher: check_existence (returning true)");
        Ok(true)
    }

    fn abort(&self) {
        tracing::debug!("StubSegmentFetcher: abort (no-op)");
    }
}
