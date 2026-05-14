# dash-rs

A Rust implementation of MPEG-DASH playback logic, inspired by
[dash.js](https://github.com/Dash-Industry-Forum/dash.js).

dash-rs provides the core building blocks needed to drive a DASH adaptive
streaming session from native Rust code.

---

## What's included

| Module | Responsibility | dash.js equivalent |
|---|---|---|
| `abr` | Adaptive bitrate rules and controller | `src/streaming/rules/` |
| `media_source` | MSE-style `MediaSource` / `SourceBuffer` traits + in-memory stub | `ExternalMediaSource.js`, `ExternalSourceBuffer.js`, `SourceBufferSink.js` |
| `fetch` | `SegmentFetcher` trait + stub | `FragmentLoader.js`, `URLLoader.js` |
| `manifest` | Manifest loading via [`dash-mpd`](https://crates.io/crates/dash-mpd) | `ManifestLoader.js` |
| `session` | Top-level `DashSession` coordinator | `MediaPlayer.js`, `StreamProcessor.js` |

### ABR rules

| Rule | Algorithm | dash.js equivalent |
|---|---|---|
| `ThroughputRule` | Picks the highest bitrate that fits within measured bandwidth × safety factor | `ThroughputRule.js` |
| `BufferOccupancyRule` | BOLA-inspired: maps buffer level to a Lyapunov utility score | `BolaRule.js` |
| `InsufficientBufferRule` | Forces lowest quality when buffer falls below 2 s | `InsufficientBufferRule.js` |
| `SwitchHistoryRule` | Prevents quality thrashing by enforcing an 8 s up-switch cooldown | `SwitchHistoryRule.js` |
| `DroppedFramesRule` | Steps down quality when the decoder drop-frame ratio exceeds a threshold | `DroppedFramesRule.js` |
| `AbandonRequestsRule` | Abandons an in-flight segment and switches down when effective download speed falls far below the current bitrate | `AbandonRequestRule.js` |

The `AbrController` runs all six rules and takes the most conservative result.

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
| wasm-pack | any recent | Required only for the `wasm_player` example. `cargo install wasm-pack` |

---

## Getting started

Add dash-rs to your `Cargo.toml`:

```toml
[dependencies]
dash-rs = { git = "https://github.com/jacksd60/dash-rs" }

# dash-rs uses async traits; bring a runtime of your choice
tokio = { version = "1", features = ["full"] }
```

### Injecting a custom clock (WASM / embedded)

`DashSession` accepts a pluggable [`Clock`] implementation for platforms where
`std::time::Instant` is unavailable:

```rust
use std::sync::Arc;
use dash_rs::{clock::Clock, session::DashSession};

struct MyPlatformClock;

// Clock must be Send + Sync; on WASM this is trivially satisfied.
unsafe impl Send for MyPlatformClock {}
unsafe impl Sync for MyPlatformClock {}

impl Clock for MyPlatformClock {
    fn now_ms(&self) -> f64  { /* platform monotonic time */ 0.0 }
    fn unix_s(&self) -> f64  { /* platform UTC time       */ 0.0 }
}

let session = DashSession::from_url_with_clock(
    &url,
    fetcher,
    media_source,
    Arc::new(MyPlatformClock),
).await?;
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
    let url = Url::parse("https://rdmedia.bbc.co.uk/bbb/2/client_manifest-common_init.mpd")?;
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

#[async_trait(?Send)]
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

#[async_trait(?Send)]
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

---

## Running the examples

### `basic` — no network required

Parses a synthetic multi-bitrate MPD, simulates 5 fetch iterations with
improving network conditions, and prints each quality decision. No network
access or external dependencies are required.

```bash
cargo run --example basic
```

Enable `tracing` debug output:

```bash
RUST_LOG=debug cargo run --example basic
```

### `http_fetch` — real HTTP, no media output

Fetches the BBC R&D Big Buck Bunny stream over HTTP and prints per-segment ABR
decisions to stdout. Requires internet access. No media sink is involved —
segments are fetched and discarded; this example exercises `DashSession` and
the full ABR pipeline end-to-end without any rendering dependency.

```bash
# Uses the BBC R&D Big Buck Bunny manifest by default
cargo run --example http_fetch

# Override the manifest URL
cargo run --example http_fetch -- https://your-stream/manifest.mpd
```

Expected output (excerpt):

```
Loaded manifest: https://rdmedia.bbc.co.uk/bbb/2/client_manifest-common_init.mpd
Video representations (sorted by bandwidth):
  [0]   256kbps  320x180
  [1]   512kbps  512x288
  ...

=== Fetching 8 segments ===
  seg   vid-idx  vid-reason    aud-idx  aud-reason    video fetch
  1     2        Throughput    0        Throughput     312 ms  (3842 kbps)
  2     3        Throughput    0        Throughput      89 ms  (5201 kbps)
  ...
```

### `wasm_player` — browser playback via WebAssembly

Runs the full MPEG-DASH session in the browser.  The architecture is a
**JS-bridge**: all browser Web API calls (MSE, Fetch, clocks) live in
`examples/wasm_player/player.js` as an annotated `bridge` object.  WASM calls
bridge methods via the `JsBridge` extern type in `src/lib.rs`; Rust never
touches browser objects directly.  Reading `player.js` top-to-bottom shows the
complete set of Web APIs involved in MPEG-DASH playback.

An on-screen **player log** panel shows all output colour-coded by source:
- Bridge calls: coloured by Web API category (MSE = blue, Fetch = green, …)
- Rust `tracing` output from WASM: coloured by severity level (warn = amber, error = red, …)

**Prerequisites:** `wasm-pack` (`cargo install wasm-pack`)

```bash
# Build the WASM module (from workspace root)
wasm-pack build examples/wasm_player --target web --out-dir examples/wasm_player/pkg

# Serve the player with any static file server
python3 -m http.server 8080 --directory examples/wasm_player
# then open http://localhost:8080
```

Pass a custom manifest URL via the `?url=` query parameter:

```
http://localhost:8080/?url=https://your-stream/manifest.mpd
```

### `gstreamer_player` — full AV playback (`sink-gstreamer` feature)

Fetches and plays back the BBC R&D Big Buck Bunny stream using GStreamer as the
media sink. Requires GStreamer ≥ 1.14 and a display.

**Prerequisites:**

```bash
# macOS
brew install gstreamer protobuf

# Debian / Ubuntu
apt-get install gstreamer1.0-tools gstreamer1.0-plugins-good \
                gstreamer1.0-plugins-bad gstreamer1.0-libav \
                protobuf-compiler
```

On macOS, if `cargo build` cannot find GStreamer headers, prepend the framework
`bin` directory so its `pkg-config` is found first:

```bash
export PATH="/Library/Frameworks/GStreamer.framework/Versions/1.0/bin:$PATH"
```

**Run:**

```bash
# Default stream (BBC R&D Big Buck Bunny)
cargo run --features sink-gstreamer --example gstreamer_player

# Override the manifest URL
cargo run --features sink-gstreamer --example gstreamer_player -- https://your-stream/manifest.mpd
```

Expected output (excerpt):

```
Loaded manifest: https://rdmedia.bbc.co.uk/bbb/2/client_manifest-common_init.mpd
Pipeline playing.

=== Fetching 8 segments (buffer target: 12 s) ===
  seg   vid-idx  vid-reason    aud-idx  aud-reason    buf-est     video fetch
  1     2        Throughput    0        Throughput       0.0 s     312 ms  (3842 kbps)
  2     3        Throughput    0        Throughput       3.8 s      89 ms  (5201 kbps)
  ...

All segments fetched — waiting for pipeline EOS…
Done.
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
│       └── quick-xml + serde (pure-Rust MPD parser)   └── BridgeFetcher       (wasm_player)
│                                              └── HttpSegmentFetcher  (http_fetch example)
├── AbrController
│   ├── ThroughputRule
│   ├── BufferOccupancyRule
│   ├── InsufficientBufferRule
│   └── SwitchHistoryRule
└── SourceBufferSink (per track)
        └── SourceBuffer (trait) ←── StubSourceBuffer
                                 └── BridgeSourceBuffer  (wasm_player)
                                 └── GstSourceBuffer     (sink-gstreamer feature)
                                 └── (your implementation)

GapController  (owned by DashSession; caller drives via session.tick_gap())
└── tick_gap(position_s, buffered, is_paused) → Option<GapSeekTarget>
    └── on gap/stall: caller seeks renderer, then calls session.seek_to()
        └── seek_to() automatically resets the gap controller (on_seek())
```

Data flow per segment:

1. `AbrController::select` picks a `QualityDecision` from `AbrMetrics`.
2. `DashSession` resolves the segment URL from the MPD `SegmentTemplate` for the current period.
3. `SegmentFetcher::fetch` retrieves the bytes.
4. `SourceBufferSink::append` forwards the `Chunk` to the `SourceBuffer`.
5. At period boundaries, `fetch_next_segment` transparently advances to the next `Period` and re-fetches init segments before continuing.

---

## Spec compliance and missing features

For a full breakdown of DASH spec compliance, behavioural differences relative
to dash.js, and the prioritised list of unimplemented features, see
[COMPLIANCE.md](COMPLIANCE.md).
