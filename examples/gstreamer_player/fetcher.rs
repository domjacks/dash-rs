//! [`HttpSegmentFetcher`] — [`dash_rs::fetch::SegmentFetcher`] implementation
//! backed by a `reqwest` HTTP client.
//!
//! After every successful fetch the measured throughput (bits per second) is
//! published into `throughput` so the driver can feed real values to the ABR
//! controller on the next iteration.

use std::{
    sync::Arc,
    time::Instant,
};

use async_trait::async_trait;
use bytes::Bytes;
use dash_rs::{
    fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher},
    ThroughputCell,
};
use url::Url;

// ── HttpSegmentFetcher ────────────────────────────────────────────────────────

pub(crate) struct HttpSegmentFetcher {
    client: reqwest::Client,
    pub(crate) throughput: Arc<ThroughputCell>,
}

impl HttpSegmentFetcher {
    pub(crate) fn new(throughput: Arc<ThroughputCell>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("dash-rs-gstreamer-example/0.1")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            throughput,
        }
    }
}

#[async_trait(?Send)]
impl SegmentFetcher for HttpSegmentFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let mut builder = self.client.get(request.url.as_str());
        if let Some((start, end)) = request.byte_range {
            builder = builder.header("Range", format!("bytes={}-{}", start, end));
        }

        let t0 = Instant::now();
        let resp = builder.send().await.map_err(|e| FetchError::Network {
            url: request.url.clone(),
            message: e.to_string(),
        })?;

        let status = resp.status().as_u16();
        let final_url = Url::parse(resp.url().as_str()).unwrap_or_else(|_| request.url.clone());

        if !resp.status().is_success() {
            return Err(FetchError::Http {
                status,
                url: request.url,
            });
        }

        let body: Bytes = resp.bytes().await.map_err(|e| FetchError::Network {
            url: final_url.clone(),
            message: e.to_string(),
        })?;

        let transfer_duration_ms = t0.elapsed().as_millis() as u64;
        let bytes_received = body.len() as u64;

        if transfer_duration_ms > 0 {
            let bps = (bytes_received as f64 * 8.0) / (transfer_duration_ms as f64 / 1_000.0);
            self.throughput.set(bps);
        }

        Ok(FetchResponse {
            final_url,
            data: body,
            transfer_duration_ms,
            bytes_received,
        })
    }

    async fn check_existence(&self, url: &Url) -> Result<bool, FetchError> {
        let resp =
            self.client
                .head(url.as_str())
                .send()
                .await
                .map_err(|e| FetchError::Network {
                    url: url.clone(),
                    message: e.to_string(),
                })?;
        Ok(resp.status().is_success())
    }

    fn abort(&self) {}
}
