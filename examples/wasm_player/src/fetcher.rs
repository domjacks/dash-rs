//! [`BridgeFetcher`] — [`dash_rs::fetch::SegmentFetcher`] implementation
//! backed by `window.fetch()` called through the [`JsBridge`].
//!
//! Byte-range requests (for `SegmentBase` and `SegmentList` with
//! `@mediaRange`) pass a `Range: bytes=<start>-<end>` header to JS, where
//! the explicit `fetch()` call is visible in `player.js`.

use async_trait::async_trait;
use bytes::Bytes;
use dash_rs::fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher};
use url::Url;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::bridge::{js_err, JsBridge};

// ── FetchResult extern type ───────────────────────────────────────────────────

/// Typed accessors for the plain JS object `{ data, finalUrl, durationMs }`
/// returned by `bridge.fetchBytes()`.
#[wasm_bindgen]
extern "C" {
    type FetchResult;

    #[wasm_bindgen(method, getter)]
    fn data(this: &FetchResult) -> js_sys::Uint8Array;

    #[wasm_bindgen(method, getter, js_name = finalUrl)]
    fn final_url(this: &FetchResult) -> String;

    #[wasm_bindgen(method, getter, js_name = durationMs)]
    fn duration_ms(this: &FetchResult) -> f64;
}

// ── BridgeFetcher ─────────────────────────────────────────────────────────────

pub(crate) struct BridgeFetcher {
    bridge: JsBridge,
}

impl BridgeFetcher {
    pub(crate) fn new(bridge: JsBridge) -> Self {
        Self { bridge }
    }
}

#[async_trait(?Send)]
impl SegmentFetcher for BridgeFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let range_header = request.byte_range.map(|(s, e)| format!("bytes={s}-{e}"));

        let resolved = JsFuture::from(self.bridge.fetch_bytes(request.url.as_str(), range_header))
            .await
            .map_err(|e| FetchError::Network {
                url: request.url.clone(),
                message: js_err(e),
            })?;

        // Cast the resolved JS object to our typed FetchResult accessor.
        let result: FetchResult = resolved.unchecked_into();
        let data = Bytes::from(result.data().to_vec());
        let bytes_received = data.len() as u64;
        let transfer_duration_ms = result.duration_ms() as u64;
        let final_url = Url::parse(&result.final_url()).unwrap_or_else(|_| request.url.clone());

        Ok(FetchResponse {
            final_url,
            data,
            transfer_duration_ms,
            bytes_received,
        })
    }

    async fn check_existence(&self, url: &Url) -> Result<bool, FetchError> {
        let ok = JsFuture::from(self.bridge.check_exists(url.as_str()))
            .await
            .map_err(|e| FetchError::Network {
                url: url.clone(),
                message: js_err(e),
            })?;
        Ok(ok.as_bool().unwrap_or(false))
    }

    fn abort(&self) {
        // No-op: dropping the in-flight JsFuture cancels the underlying fetch
        // in most browsers when no AbortController is attached.
    }
}
