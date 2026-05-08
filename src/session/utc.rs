//! UTC clock synchronisation via `UTCTiming` elements in the MPD.
//!
//! Implements the client-side of the `TimeSyncController.js` role from
//! dash.js.  After loading a live manifest, `DashSession::from_url` calls
//! [`resolve_utc_timing`] to compute a signed clock offset
//! (`server_time − client_time`) that is then applied to every live-edge
//! segment calculation.
//!
//! # Supported schemes
//!
//! | `@schemeIdUri` | How the server time is obtained |
//! |---|---|
//! | `urn:mpeg:dash:utc:direct:2014` | Parse `@value` as an XS datetime — no network request needed |
//! | `urn:mpeg:dash:utc:http-xsdate:2014` | GET `@value` URL; parse body as XS datetime |
//! | `urn:mpeg:dash:utc:http-iso:2014` | GET `@value` URL; parse body as ISO 8601 (same parser) |
//! | `urn:mpeg:dash:utc:http-head:2014` | Requires response headers — **not supported**; skipped with a warning |
//! | `urn:mpeg:dash:utc:ntp:2014` | Requires raw UDP NTP — **not supported**; skipped with a warning |
//!
//! The resolver tries elements in manifest order and stops at the first
//! success.  If every element either fails or is unsupported, the offset
//! is left at `0.0` (system clock used as-is).

use std::sync::Arc;

use crate::manifest::mpd_types::MPD;
use url::Url;

use crate::{
    fetch::{FetchRequest, RequestType, SegmentFetcher},
    util::parse_xs_datetime,
};

// Scheme URI constants.
const SCHEME_DIRECT: &str = "urn:mpeg:dash:utc:direct:2014";
const SCHEME_HTTP_XSDATE: &str = "urn:mpeg:dash:utc:http-xsdate:2014";
const SCHEME_HTTP_ISO: &str = "urn:mpeg:dash:utc:http-iso:2014";
const SCHEME_HTTP_HEAD: &str = "urn:mpeg:dash:utc:http-head:2014";
const SCHEME_NTP: &str = "urn:mpeg:dash:utc:ntp:2014";

/// Resolve the UTC clock offset (in seconds) from `MPD.UTCTiming` elements.
///
/// Returns `server_time_s − client_now_s`, where `client_now_s` is
/// `SystemTime::now()` at the moment the server's time is obtained.
///
/// Returns `0.0` if no `UTCTiming` element is present, all elements fail, or
/// no supported scheme is found.
pub(crate) async fn resolve_utc_timing<F: SegmentFetcher>(
    mpd: &MPD,
    fetcher: &Arc<F>,
    client_now_s: f64,
) -> f64 {
    for entry in &mpd.UTCTiming {
        let scheme = entry.schemeIdUri.as_str();

        match scheme {
            SCHEME_DIRECT => {
                if let Some(value) = &entry.value {
                    if let Some(server_s) = parse_xs_datetime(value) {
                        let offset = server_s - client_now_s;
                        tracing::info!(
                            scheme = SCHEME_DIRECT,
                            offset_s = offset,
                            "UTCTiming: clock offset resolved"
                        );
                        return offset;
                    }
                    tracing::warn!(
                        scheme = SCHEME_DIRECT,
                        value = %value,
                        "UTCTiming: failed to parse direct value — trying next element"
                    );
                } else {
                    tracing::warn!(
                        scheme = SCHEME_DIRECT,
                        "UTCTiming: direct element has no @value — trying next element"
                    );
                }
            }

            SCHEME_HTTP_XSDATE | SCHEME_HTTP_ISO => {
                if let Some(url_str) = &entry.value {
                    match Url::parse(url_str) {
                        Ok(url) => {
                            if let Some(offset) =
                                query_http_utc(&url, fetcher, client_now_s).await
                            {
                                tracing::info!(
                                    scheme,
                                    url = %url_str,
                                    offset_s = offset,
                                    "UTCTiming: clock offset resolved"
                                );
                                return offset;
                            }
                            tracing::warn!(
                                scheme,
                                url = %url_str,
                                "UTCTiming: HTTP query failed — trying next element"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                scheme,
                                url = %url_str,
                                error = %e,
                                "UTCTiming: invalid URL — trying next element"
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        scheme,
                        "UTCTiming: HTTP element has no @value URL — trying next element"
                    );
                }
            }

            SCHEME_HTTP_HEAD => {
                tracing::warn!(
                    scheme = SCHEME_HTTP_HEAD,
                    "UTCTiming: http-head scheme requires response headers \
                     and is not yet supported — trying next element"
                );
            }

            SCHEME_NTP => {
                tracing::warn!(
                    scheme = SCHEME_NTP,
                    "UTCTiming: NTP scheme requires raw UDP and is not yet \
                     supported — trying next element"
                );
            }

            other => {
                tracing::warn!(
                    scheme = other,
                    "UTCTiming: unrecognised scheme — trying next element"
                );
            }
        }
    }

    0.0
}

/// Fetch the UTCTiming endpoint and parse the response body as an XS datetime.
///
/// Returns `server_time_s − client_now_s` on success, `None` on any error.
async fn query_http_utc<F: SegmentFetcher>(
    url: &Url,
    fetcher: &Arc<F>,
    client_now_s: f64,
) -> Option<f64> {
    let request = FetchRequest::new(url.clone(), RequestType::ClockSync);
    let response = fetcher.fetch(request).await.ok()?;

    let body = std::str::from_utf8(&response.data).ok()?.trim().to_owned();
    let server_s = parse_xs_datetime(&body)?;
    Some(server_s - client_now_s)
}
