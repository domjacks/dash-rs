pub mod stub;

use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;
use url::Url;

// ── Error ─────────────────────────────────────────────────────────────────────

/// Errors that can occur during a fetch operation.
#[derive(Debug, Error)]
pub enum FetchError {
    #[error("HTTP error {status} fetching {url}")]
    Http { status: u16, url: Url },

    #[error("network error fetching {url}: {message}")]
    Network { url: Url, message: String },

    #[error("request was aborted")]
    Aborted,

    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}

// ── Request / Response types ──────────────────────────────────────────────────

/// The type of resource being fetched.
///
/// Mirrors the `HTTPRequest` type constants from `dash.js`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestType {
    /// An MPD manifest (`.mpd` file).
    Manifest,
    /// A media initialisation segment (no presentation time).
    InitializationSegment,
    /// A regular media segment.
    MediaSegment,
    /// A UTC clock-synchronisation endpoint (`UTCTiming` element).
    ///
    /// Used by `urn:mpeg:dash:utc:http-xsdate:2014` and
    /// `urn:mpeg:dash:utc:http-iso:2014` schemes.  The response body is
    /// expected to contain a UTC datetime string.
    ClockSync,
}

/// A request for a single network resource.
///
/// Mirrors the fields on dash.js's internal request value objects
/// (`TextRequest`, `FragmentRequest`).
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// The URL to retrieve.
    pub url: Url,
    /// Optional byte-range for partial requests (`Range: bytes=start-end`).
    pub byte_range: Option<(u64, u64)>,
    /// Classification of this request for metrics and logging.
    pub request_type: RequestType,
}

impl FetchRequest {
    pub fn new(url: Url, request_type: RequestType) -> Self {
        Self {
            url,
            byte_range: None,
            request_type,
        }
    }

    pub fn with_byte_range(mut self, start: u64, end: u64) -> Self {
        self.byte_range = Some((start, end));
        self
    }
}

/// The result of a successful fetch.
#[derive(Debug)]
pub struct FetchResponse {
    /// The final URL after any redirects. Used to resolve relative URLs in
    /// manifests (mirrors dash.js's `responseURL` handling).
    pub final_url: Url,
    /// The response body.
    pub data: Bytes,
    /// Wall-clock time the transfer took in milliseconds.
    /// Used by `ThroughputRule` to compute download speed.
    pub transfer_duration_ms: u64,
    /// Number of bytes received (may differ from `data.len()` if compressed).
    pub bytes_received: u64,
}

impl FetchResponse {
    /// Compute throughput in bits per second from the response metadata.
    /// Returns `None` if `transfer_duration_ms` is zero.
    pub fn throughput_bps(&self) -> Option<f64> {
        if self.transfer_duration_ms == 0 {
            return None;
        }
        let bytes = self.bytes_received as f64;
        let secs = self.transfer_duration_ms as f64 / 1_000.0;
        Some((bytes * 8.0) / secs)
    }
}

// ── SegmentFetcher trait ──────────────────────────────────────────────────────

/// Abstracts over the network layer used to download DASH resources.
///
/// Mirrors the role of `URLLoader.js` + `FragmentLoader.js` + `ManifestLoader.js`
/// in dash.js. The stub implementation ([`stub::StubSegmentFetcher`]) logs
/// every call and returns empty bytes immediately.
///
/// A real HTTP implementation can be provided by the caller or by a future
/// `fetch-http` crate feature.
#[async_trait(?Send)]
pub trait SegmentFetcher {
    /// Fetch a resource and return the response.
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError>;

    /// Issue a `HEAD` request to check whether a URL exists without
    /// downloading the body. Mirrors `FragmentLoader.checkForExistence()`.
    async fn check_existence(&self, url: &Url) -> Result<bool, FetchError>;

    /// Issue a `HEAD` request to `url` and return the value of the `Date:`
    /// response header, if present.
    ///
    /// Used for the `urn:mpeg:dash:utc:http-head:2014` `UTCTiming` scheme
    /// (DASH spec §5.8.5.7): the server's current UTC time is read from the
    /// `Date:` header of a `HEAD` response, avoiding a body download.
    ///
    /// The default implementation returns `Ok(None)` — override it in
    /// concrete implementations that support `HEAD` requests with header
    /// access (e.g. a `reqwest`-backed fetcher).  When `Ok(None)` is returned
    /// the `http-head` scheme is skipped and the resolver tries the next
    /// `UTCTiming` element in the manifest.
    async fn head_date(&self, url: &Url) -> Result<Option<String>, FetchError> {
        let _ = url;
        Ok(None)
    }

    /// Cancel any in-flight requests. Mirrors `FragmentLoader.abort()`.
    /// Implementations are expected to be cancel-safe; if the future returned
    /// by `fetch` is dropped, the request should also be cancelled.
    fn abort(&self);
}
