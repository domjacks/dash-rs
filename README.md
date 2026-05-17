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

To play a DASH stream you need to provide three trait implementations and a
driver struct, then call two functions. The library handles ABR, buffer gating,
live manifest refresh, multi-period transitions, gap detection, and
end-of-stream signalling.

| What to implement | Trait | Role |
|---|---|---|
| Segment fetcher | `SegmentFetcher` | Downloads manifests and media segments |
| Media buffer | `SourceBuffer` | Receives raw fMP4/WebM bytes; `append_buffer` feeds them to your decoder |
| Media container | `MediaSource` | Creates `SourceBuffer` instances and signals end-of-stream |
| Playback driver | `PlaybackDriver` | Supplies the renderer's position and `PlaybackState` (`Playing` / `Paused` / `Buffering`) to the fetch loop |

### 1. Add the dependency

```toml
[dependencies]
dash-rs     = { git = "https://github.com/jacksd60/dash-rs" }
async-trait = "0.1"
bytes       = "1"
reqwest     = { version = "0.12", features = ["stream"] }
tokio       = { version = "1", features = ["full"] }
```

### 2. Implement `SegmentFetcher`

Called by the session to download every manifest and media segment.

```rust
use std::time::Instant;
use async_trait::async_trait;
use dash_rs::fetch::{FetchError, FetchRequest, FetchResponse, SegmentFetcher};
use url::Url;

struct HttpFetcher {
    client: reqwest::Client,
}

impl HttpFetcher {
    fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("my-player/0.1")
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
        }
    }
}

#[async_trait(?Send)]
impl SegmentFetcher for HttpFetcher {
    async fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, FetchError> {
        let mut builder = self.client.get(req.url.as_str());
        if let Some((start, end)) = req.byte_range {
            builder = builder.header("Range", format!("bytes={start}-{end}"));
        }
        let t0   = Instant::now();
        let resp = builder
            .send()
            .await
            .map_err(|e| FetchError::Network { url: req.url.clone(), message: e.to_string() })?;
        let status = resp.status().as_u16();
        if status != 200 && status != 206 {
            return Err(FetchError::Http { status, url: req.url });
        }
        let data = resp
            .bytes()
            .await
            .map_err(|e| FetchError::Network { url: req.url.clone(), message: e.to_string() })?;
        Ok(FetchResponse {
            final_url:            req.url,
            transfer_duration_ms: t0.elapsed().as_millis() as u64,
            bytes_received:       data.len() as u64,
            data,
        })
    }

    async fn check_existence(&self, url: &Url) -> Result<bool, FetchError> {
        let resp = self.client
            .head(url.as_str())
            .send()
            .await
            .map_err(|e| FetchError::Network { url: url.clone(), message: e.to_string() })?;
        Ok(resp.status().is_success())
    }

    fn abort(&self) {}
}
```

### 3. Implement `SourceBuffer` and `MediaSource`

`SourceBuffer` is where your decoder integration lives.  Implement
`append_buffer` to forward `chunk.data` to your pipeline — every other method
is bookkeeping used by the session.

`MediaSource` creates `SourceBuffer` instances (one per track) and signals
end-of-stream to the renderer.

```rust
use async_trait::async_trait;
use dash_rs::{
    error::DashError,
    media_source::{Chunk, MediaSource, ReadyState, SourceBuffer, TimeRange},
};

// ── SourceBuffer ──────────────────────────────────────────────────────────────

struct MySourceBuffer {
    mime:      String,
    ts_offset: f64,
    win_start: f64,
    win_end:   f64,
}

impl MySourceBuffer {
    fn new(mime: &str) -> Self {
        Self {
            mime:      mime.to_string(),
            ts_offset: 0.0,
            win_start: 0.0,
            win_end:   f64::INFINITY,
        }
    }
}

#[async_trait(?Send)]
impl SourceBuffer for MySourceBuffer {
    fn mime_type(&self)           -> &str           { &self.mime }
    fn is_updating(&self)         -> bool           { false }
    fn buffered(&self)            -> Vec<TimeRange> { vec![] }
    fn timestamp_offset(&self)    -> f64            { self.ts_offset }
    fn set_timestamp_offset(&mut self, v: f64)      { self.ts_offset = v; }
    fn append_window_start(&self) -> f64            { self.win_start }
    fn set_append_window_start(&mut self, v: f64)   { self.win_start = v; }
    fn append_window_end(&self)   -> f64            { self.win_end }
    fn set_append_window_end(&mut self, v: f64)     { self.win_end = v; }

    async fn append_buffer(&mut self, chunk: Chunk) -> Result<(), DashError> {
        // Send chunk.data to your decoder here.
        // chunk.label identifies the track, e.g. "video/init" or "audio/seg-3".
        let _ = chunk;
        Ok(())
    }

    async fn remove(&mut self, _: f64, _: f64) -> Result<(), DashError> { Ok(()) }
    async fn abort(&mut self)                   -> Result<(), DashError> { Ok(()) }
    async fn change_type(&mut self, mime: &str) -> Result<(), DashError> {
        self.mime = mime.to_string();
        Ok(())
    }
}

// ── MediaSource ───────────────────────────────────────────────────────────────

struct MyMediaSource {
    duration: Option<f64>,
}

impl MyMediaSource {
    fn new() -> Self { Self { duration: None } }
}

#[async_trait(?Send)]
impl MediaSource for MyMediaSource {
    fn ready_state(&self) -> ReadyState  { ReadyState::Open }
    fn duration(&self)    -> Option<f64> { self.duration }

    fn set_duration(&mut self, d: f64) -> Result<(), DashError> {
        self.duration = Some(d);
        Ok(())
    }

    fn add_source_buffer(&mut self, mime_type: &str) -> Result<Box<dyn SourceBuffer>, DashError> {
        Ok(Box::new(MySourceBuffer::new(mime_type)))
    }

    fn remove_source_buffer(&mut self, _: &str) -> Result<(), DashError> { Ok(()) }

    async fn end_of_stream(&mut self) -> Result<(), DashError> {
        // Signal your renderer that all media has been delivered.
        Ok(())
    }

    fn close(&mut self) {}
}
```

### 4. Implement `PlaybackDriver`

`PlaybackDriver` is how `run()` reads your renderer's state.  `position_s` is
the only required method — every other method has a sensible default.

```rust
use async_trait::async_trait;
use dash_rs::session::driver::{PlaybackDriver, PlaybackState};

struct MyDriver {
    // Your renderer handle, or nothing for a headless setup.
}

#[async_trait(?Send)]
impl PlaybackDriver for MyDriver {
    /// Current playback position in seconds.
    /// For a headless setup (no real renderer) returning 0.0 is fine.
    fn position_s(&self) -> f64 {
        0.0 // replace with: self.renderer.current_position_s()
    }

    /// Current renderer state.
    ///
    /// - `Paused`    — user-paused; freezes the buffer-level wall-clock and
    ///                 suppresses gap-jump seeks.
    /// - `Buffering` — renderer stalled waiting for data (not a user action);
    ///                 same effect as `Paused` for clock and gap purposes.
    /// - `Playing`   — default; clock runs freely.
    fn playback_state(&self) -> PlaybackState {
        PlaybackState::Playing // replace with your renderer's actual state
    }
}
```

Override additional methods as needed:

| Method | Default | When to override |
|---|---|---|
| `playback_state()` | `Playing` | Report `Paused` or `Buffering` to freeze the clock and suppress gap jumps |
| `on_gap_seek(target)` | no-op | Seek the renderer when the gap controller fills a stall |
| `throughput_override_bps()` | `None` (internal tracker) | Feed externally measured bandwidth to the ABR controller |
| `audio_gate_target_ms(vbuf, default)` | `default` | Slave audio buffering to video level (e.g. GStreamer) |
| `enable_pruning()` | `false` | Evict old buffer data on native renderers |
| `should_continue()` | `true` | Exit the fetch loop early |
| `on_iteration_start(pos)` | no-op | Advance per-iteration state before a fetch |
| `on_iteration(summary)` | no-op | Progress reporting and UI updates after each segment pair |

### 5. Start playback

The simplest path uses the `Player` convenience wrapper, which owns both the
session and the driver:

```rust
use url::Url;
use dash_rs::Player;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = Url::parse("https://example.com/stream.mpd")?;

    // from_url fetches + parses the MPD, opens the media source, and returns
    // a Player ready to run.
    let mut player = Player::from_url(
        &url,
        HttpFetcher::new(),
        MyMediaSource::new(),
        MyDriver::new(),
    ).await?;

    // Inspect representations before starting the loop if needed.
    for rep in player.session().video_representations() {
        println!("{} kbps", rep.bandwidth / 1_000);
    }

    // run() handles ABR, buffer gating, gap detection, live manifest refresh,
    // multi-period transitions, and end-of-stream.
    player.run().await?;

    Ok(())
}
```

When the manifest is already in memory (e.g. in tests or offline scenarios),
use `Player::from_mpd` instead:

```rust
use std::sync::Arc;
use dash_rs::{manifest, Player};

let mpd = manifest::parse(mpd_xml)?;
let mut player = Player::from_mpd(
    mpd,
    base_url,
    Arc::new(MyFetcher::new()),
    MyMediaSource::new(),
    MyDriver::new(),
)?;
player.run().await?;
```

| Constructor | When to use |
|---|---|
| `Player::from_url` | URL known at startup; uses `SystemClock` (requires `std`) |
| `Player::from_url_with_clock` | URL known at startup; custom clock (WASM / no `std`) |
| `Player::from_mpd` | Manifest already parsed; uses `SystemClock` (requires `std`) |
| `Player::from_mpd_with_clock` | Manifest already parsed; custom clock (WASM / no `std`) |

`from_url` fetches and parses the MPD, resolves XLink references, syncs the UTC
clock for live streams, and snaps to the live edge.  `run()` returns `Ok(())`
once `end_of_stream()` has been called on your `MediaSource`.

For platforms without `std::time::Instant` (e.g. WASM), use
`from_url_with_clock` and pass a [`Clock`] implementation — see
[Injecting a custom clock](#injecting-a-custom-clock) below.

### Seeking

Gap-fill seeks (skipping over stalls or missing segments) are delivered to your
driver via `on_gap_seek` while `run()` is active — override the method to seek
your renderer.

For user-initiated seeks, interrupt the loop by returning `false` from
`should_continue()`, then seek and restart:

```rust
// Exit the loop cleanly.
driver.stop();                             // sets should_continue() → false
session.run(&mut driver).await?;           // drains current iteration and returns

// Reposition the fetch cursor to the nearest segment boundary.
let snapped_s = session.seek_to(target_s);

// Restart the playing clock and resume fetching.
session.notify_playing();
session.run(&mut driver).await?;
```

`seek_to` snaps to the nearest segment boundary, returns the snapped position,
and automatically resets the gap controller.

### Injecting a custom clock (WASM / embedded)

`DashSession` accepts a pluggable [`Clock`] implementation for platforms where
`std::time::Instant` is unavailable.  The trait has three methods:

| Method | Used for |
|---|---|
| `now_ms()` | Monotonic elapsed time; drives buffer-level estimation and pause accounting |
| `unix_s()` | UTC wall-clock time; drives live-edge snapping and `UTCTiming` sync |
| `set_timeout(ms)` | One-shot async timer; used by `wait_for_buffer_capacity` |

```rust
use std::sync::Arc;
use async_trait::async_trait;
use dash_rs::clock::Clock;

struct MyPlatformClock;

// Clock must be Send + Sync; on WASM this is satisfied via unsafe impl.
unsafe impl Send for MyPlatformClock {}
unsafe impl Sync for MyPlatformClock {}

#[async_trait(?Send)]
impl Clock for MyPlatformClock {
    fn now_ms(&self) -> f64  { /* platform monotonic time in ms */ 0.0 }
    fn unix_s(&self) -> f64  { /* platform UTC time in seconds  */ 0.0 }

    async fn set_timeout(&self, ms: f64) {
        // Platform sleep — e.g. futures_timer::Delay on native,
        // JsFuture::from(bridge.sleep(ms)) on WASM.
        futures_timer::Delay::new(std::time::Duration::from_millis(ms as u64)).await;
    }
}

let session = DashSession::from_url_with_clock(
    &url,
    fetcher,
    media_source,
    Arc::new(MyPlatformClock),
).await?;
```

`SystemClock` (the default) uses `std::time::Instant` and `futures_timer::Delay`
and works with any async executor without requiring tokio.

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

## Inspecting session state

Access session state at any point — before, during (via `on_iteration_start` or
`on_iteration` in your `PlaybackDriver`), or after `run()`.

```rust
use dash_rs::session::MediaType;

// Available representations, sorted ascending by bandwidth.
for (i, rep) in session.video_representations().iter().enumerate() {
    println!("[{i}] {}kbps {}×{}", rep.bandwidth / 1000,
        rep.width.unwrap_or(0), rep.height.unwrap_or(0));
}

// Currently active representation.
if let Some(rep) = session.current_representation(&MediaType::Video) {
    println!("playing at {}kbps", rep.bandwidth / 1000);
}

// Estimated buffer level.
let buf_ms = session.estimated_buffer_level_ms(&MediaType::Video);
println!("video buffer: {:.1} s", buf_ms / 1000.0);

// Cap quality, e.g. to enforce a data-saver mode.
session.set_max_bitrate(MediaType::Video, 2_000_000);
session.clear_max_bitrate(MediaType::Video);

// Current period in a multi-period stream.
println!("period {}/{}", session.current_period_index() + 1, session.period_count());
```

The `on_iteration` callback in `PlaybackDriver` receives an [`IterationSummary`]
after every segment pair, which carries the same values without requiring a
reference to the session:

```rust
fn on_iteration(&mut self, s: &IterationSummary<'_>) {
    println!(
        "video={:?} buf={:.1}s  audio={:?} buf={:.1}s  throughput={:.0}kbps",
        s.video_decision.map(|d| d.index),
        s.video_buffer_ms / 1_000.0,
        s.audio_decision.map(|d| d.index),
        s.audio_buffer_ms / 1_000.0,
        s.last_throughput_bps / 1_000.0,
    );
}
```

---

## Subscribing to events

```rust
use dash_rs::session::DashEvent;

let mut events = session.subscribe_events();
tokio::spawn(async move {
    while let Ok(event) = events.recv().await {
        match event {
            DashEvent::RepresentationSwitch { media_type, from_index, to_index, .. } => {
                println!("{media_type:?} switched {from_index} → {to_index}");
            }
            DashEvent::InbandEvent { scheme_id_uri, value, data, .. } => {
                // DASH emsg events (e.g. splice cues, ID3 metadata).
                println!("emsg {scheme_id_uri} {value:?}: {} bytes", data.len());
            }
            _ => {}
        }
    }
});
```

`subscribe_events` returns a `broadcast::Receiver`; events are dispatched by
`fetch_next_segment` from within the fetch loop.

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
Loaded manifest: https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd
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
Loaded manifest: https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd
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
└── tick_gap(position_s, buffered, PlaybackState) → Option<GapSeekTarget>
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
