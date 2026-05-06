# dash-rs

A Rust implementation of MPEG-DASH playback logic, inspired by
[dash.js](https://github.com/Dash-Industry-Forum/dash.js).

dash-rs provides the core building blocks needed to drive a DASH adaptive
streaming session from native Rust code — without requiring a browser or any
JavaScript runtime.

---

## What's included

| Module | Responsibility | dash.js equivalent |
|---|---|---|
| `abr` | Adaptive bitrate rules and controller | `src/streaming/rules/` |
| `media_source` | MSE-style `MediaSource` / `SourceBuffer` traits + in-memory stub | `ExternalMediaSource.js`, `ExternalSourceBuffer.js`, `SourceBufferSink.js` |
| `fetch` | `SegmentFetcher` trait + stub (no real HTTP yet) | `FragmentLoader.js`, `URLLoader.js` |
| `manifest` | Manifest loading via [`dash-mpd`](https://crates.io/crates/dash-mpd) | `ManifestLoader.js` |
| `session` | Top-level `DashSession` coordinator | `MediaPlayer.js`, `StreamProcessor.js` |

### ABR rules

| Rule | Algorithm | dash.js equivalent |
|---|---|---|
| `ThroughputRule` | Picks the highest bitrate that fits within measured bandwidth × safety factor | `ThroughputRule.js` |
| `BufferOccupancyRule` | BOLA-inspired: maps buffer level to a Lyapunov utility score | `BolaRule.js` |
| `InsufficientBufferRule` | Forces lowest quality when buffer falls below 2 s | `InsufficientBufferRule.js` |
| `SwitchHistoryRule` | Prevents quality thrashing by enforcing an 8 s up-switch cooldown | `SwitchHistoryRule.js` |

The `AbrController` runs all four rules and takes the most conservative result.

### MSE abstraction

Where dash.js calls browser MSE APIs (`appendBuffer`, `remove`, `addSourceBuffer`, `endOfStream`, …), dash-rs exposes equivalent async Rust traits:

- **`MediaSource`** — mirrors the browser `MediaSource` object.
- **`SourceBuffer`** — mirrors the browser `SourceBuffer` object.
- **`SourceBufferSink`** — a queuing wrapper around `SourceBuffer`, mirrors `SourceBufferSink.js`.

The `stub` module ships a fully in-memory implementation (`StubMediaSource` /
`StubSourceBuffer`) that resolves every call immediately. Plug in your own
implementation (GStreamer `appsrc`, a custom demuxer, etc.) by implementing the
two traits.

---

## Prerequisites

| Tool | Version | Notes |
|---|---|---|
| Rust | stable ≥ 1.75 | Earlier versions may work but are untested |
| protoc | any recent | Required by the `pssh-box` transitive dependency of `dash-mpd`. On macOS: `brew install protobuf` |

---

## Getting started

Add dash-rs to your `Cargo.toml`:

```toml
[dependencies]
dash-rs = { git = "https://github.com/jacksd60/dash-rs" }

# dash-rs uses async traits; bring a runtime of your choice
tokio = { version = "1", features = ["full"] }
```

### Minimal example

```rust
use std::sync::Arc;
use url::Url;
use dash_rs::{
    abr::AbrMetrics,
    fetch::stub::StubSegmentFetcher,
    media_source::stub::StubMediaSource,
    session::{DashSession, MediaType},
};

#[tokio::main]
async fn main() -> Result<(), dash_rs::DashError> {
    // 1. Open a media sink (replace with your real MediaSource implementation).
    let mut media_source = StubMediaSource::new();
    media_source.open();

    // 2. Load the manifest and start a session.
    //    Use StubSegmentFetcher for now; replace with an HTTP fetcher later.
    let url = Url::parse("https://dash.akamaized.net/envivio/EnvivioDash3/manifest.mpd")?;
    let mut session = DashSession::from_url(
        &url,
        StubSegmentFetcher,
        media_source,
    ).await?;

    // 3. Inspect what's in the manifest.
    for (i, rep) in session.video_representations().iter().enumerate() {
        println!("[{i}] {}kbps {}x{}", rep.bandwidth / 1000,
            rep.width.unwrap_or(0), rep.height.unwrap_or(0));
    }

    // 4. Drive the fetch loop.
    //    Supply real throughput / buffer measurements in a production app.
    let metrics = AbrMetrics::new(
        4_000_000.0,  // 4 Mbps measured throughput
        10_000.0,     // 10 s of buffer already filled
    );
    let decision = session.fetch_next_segment(MediaType::Video, &metrics).await?;
    println!("fetched at quality index {} ({:?})", decision.index, decision.reason);

    session.end_of_stream().await?;
    Ok(())
}
```

### Providing your own `MediaSource`

Implement the two traits to connect dash-rs to your rendering backend:

```rust
use async_trait::async_trait;
use dash_rs::{
    error::DashError,
    media_source::{Chunk, MediaSource, ReadyState, SourceBuffer, TimeRange},
};

struct MySourceBuffer { /* ... */ }

#[async_trait]
impl SourceBuffer for MySourceBuffer {
    fn mime_type(&self) -> &str { "video/mp4" }
    fn is_updating(&self) -> bool { false }
    fn buffered(&self) -> Vec<TimeRange> { vec![] }
    fn timestamp_offset(&self) -> f64 { 0.0 }
    fn set_timestamp_offset(&mut self, _: f64) {}
    fn append_window_start(&self) -> f64 { 0.0 }
    fn set_append_window_start(&mut self, _: f64) {}
    fn append_window_end(&self) -> f64 { f64::INFINITY }
    fn set_append_window_end(&mut self, _: f64) {}

    async fn append_buffer(&mut self, chunk: Chunk) -> Result<(), DashError> {
        // Feed chunk.data to your decoder / renderer here.
        Ok(())
    }
    async fn remove(&mut self, _start: f64, _end: f64) -> Result<(), DashError> { Ok(()) }
    async fn abort(&mut self) -> Result<(), DashError> { Ok(()) }
    async fn change_type(&mut self, _mime: &str) -> Result<(), DashError> { Ok(()) }
}
```

### Providing your own `SegmentFetcher`

```rust
use async_trait::async_trait;
use dash_rs::fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher};
use url::Url;

struct HttpFetcher { client: reqwest::Client }

#[async_trait]
impl SegmentFetcher for HttpFetcher {
    async fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, FetchError> {
        let resp = self.client.get(req.url.as_str()).send().await
            .map_err(|e| FetchError::Network { url: req.url.clone(), message: e.to_string() })?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(FetchError::Http { status, url: req.url });
        }
        let data = resp.bytes().await
            .map_err(|e| FetchError::Network { url: req.url.clone(), message: e.to_string() })?;
        let bytes_received = data.len() as u64;
        Ok(FetchResponse { final_url: req.url, data, transfer_duration_ms: 0, bytes_received })
    }

    async fn check_existence(&self, url: &Url) -> Result<bool, FetchError> {
        let resp = self.client.head(url.as_str()).send().await
            .map_err(|e| FetchError::Network { url: url.clone(), message: e.to_string() })?;
        Ok(resp.status().is_success())
    }

    fn abort(&self) {}
}
```

### Custom ABR rules

```rust
use dash_rs::abr::{metrics::AbrMetrics, rule::{AbrRule, RepresentationInfo}, AbrController};

struct MyRule;

impl AbrRule for MyRule {
    fn name(&self) -> &str { "MyRule" }

    fn max_index(&self, reps: &[RepresentationInfo], _metrics: &AbrMetrics) -> Option<usize> {
        // Always cap at the second-highest quality.
        Some(reps.len().saturating_sub(2))
    }
}

let controller = AbrController::with_rules(vec![Box::new(MyRule)]);
```

---

## Running the tests

```bash
# Run all unit + integration + doc tests
cargo test

# Run only ABR rule tests
cargo test --test abr_rules

# Run only MSE abstraction tests
cargo test --test media_source

# Run only doc-tests
cargo test --doc
```

Expected output (all 27 tests pass):

```
running 11 tests
test buffer_rule_abstains_when_no_buffer ... ok
test buffer_rule_returns_some_when_buffer_present ... ok
test controller_empty_representations ... ok
test controller_picks_conservative_result ... ok
test controller_selects_high_with_good_conditions ... ok
test insufficient_buffer_abstains_above_threshold ... ok
test insufficient_buffer_fires_below_threshold ... ok
test switch_history_abstains_with_no_history ... ok
test switch_history_clamps_after_recent_switch ... ok
test throughput_rule_empty_representations ... ok
test throughput_rule_selects_highest_fitting_quality ... ok

test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured

running 14 tests
test source_buffer_sink_append_and_abort ... ok
test source_buffer_sink_remove_validates_range ... ok
test stub_media_source_add_and_remove_source_buffer ... ok
test stub_media_source_add_source_buffer_requires_open ... ok
test stub_media_source_end_of_stream ... ok
test stub_media_source_open_close ... ok
test stub_media_source_set_duration ... ok
test stub_source_buffer_abort_clears_updating ... ok
test stub_source_buffer_append_and_buffered ... ok
test stub_source_buffer_change_type ... ok
test stub_source_buffer_init_segment_not_tracked_in_time_ranges ... ok
test stub_source_buffer_keeps_discontinuous_ranges_separate ... ok
test stub_source_buffer_merges_adjacent_ranges ... ok
test stub_source_buffer_remove_clears_range ... ok

test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured
```

---

## Running the example

The `basic` example parses a synthetic multi-bitrate MPD, simulates 5 fetch
iterations with improving network conditions, and prints each quality decision.
No network access is required.

```bash
cargo run --example basic
```

Enable `tracing` debug output (shown by default in the example):

```bash
RUST_LOG=debug cargo run --example basic
```

---

## Enabling tracing

dash-rs emits structured `tracing` events at `DEBUG` and `INFO` levels from
every subsystem. To see them in your application, add a subscriber:

```toml
[dev-dependencies]
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

```rust
tracing_subscriber::fmt()
    .with_env_filter("dash_rs=debug")
    .init();
```

---

## Architecture

```
DashSession
├── ManifestLoader ──── SegmentFetcher (trait) ←── StubSegmentFetcher
│       └── dash-mpd (MPD parse)
├── AbrController
│   ├── ThroughputRule
│   ├── BufferOccupancyRule
│   ├── InsufficientBufferRule
│   └── SwitchHistoryRule
└── SourceBufferSink (per track)
        └── SourceBuffer (trait) ←── StubSourceBuffer
                                 └── (your implementation)
```

Data flow per segment:

1. `AbrController::select` picks a `QualityDecision` from `AbrMetrics`.
2. `DashSession` resolves the segment URL from the MPD `SegmentTemplate`.
3. `SegmentFetcher::fetch` retrieves the bytes.
4. `SourceBufferSink::append` forwards the `Chunk` to the `SourceBuffer`.

---

## What's not yet implemented

- Real HTTP fetching (bring your own `SegmentFetcher`, or wait for the planned `fetch-http` feature).
- Live / dynamic MPD refresh (`ManifestUpdater`).
- `SegmentList` and `SegmentBase` (byte-range) addressing — only `SegmentTemplate` with `$Number$` is currently supported.
- Multi-period playback.
- Subtitle / text track handling.
- DRM / Encrypted Media Extensions.
