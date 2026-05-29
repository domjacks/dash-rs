# dash-rs

A Rust implementation of MPEG-DASH playback logic, inspired by
[dash.js](https://github.com/Dash-Industry-Forum/dash.js).

dash-rs provides the core building blocks needed to drive a DASH adaptive
streaming session from native Rust code.

---

## What's included

- VOD/AOD playback
- Live playback
- Seeking
- ABR (see rules below)
- Playback metrics
- Logging
- WASM + GStreamer examples (see below for how to run)

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

## Prerequisites

| Tool | Version | Notes |
|---|---|---|
| Rust | stable ≥ 1.75 | Earlier versions may work but are untested |
| wasm-pack | any recent | Required only for the `wasm_player` example. `cargo install wasm-pack` |

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

## Player events

```rust
use dash_rs::PlayerEvent;

let mut player = Player::from_url(&url, fetcher, buffer, driver).await?;
player.on(|event| {
    match event {
        PlayerEvent::Log(log) => {
            println!("[{:?}] {}: {}", log.level, log.target, log.message);
        }
        PlayerEvent::Iteration(s) => {
            println!("buffer={:.1}s throughput={:.0}kbps", s.video_buffer_ms / 1_000.0, s.last_throughput_bps.unwrap_or(0.0) / 1_000.0);
        }
        PlayerEvent::InbandDash(ev) => {
            // DASH emsg events (e.g. splice cues, ID3 metadata).
            println!("emsg {} {:?}: {} bytes", ev.scheme_id_uri, ev.value, ev.message_data.len());
        }
        PlayerEvent::PlaybackStateChanged { from, to } => {
            println!("playback state {from:?} -> {to:?}");
        }
        PlayerEvent::QualityChange { media_type, old_index, new_index, .. } => {
            println!("{media_type:?} switched {old_index} -> {new_index}");
        }
        _ => {}
    }
});

player.run().await?;
```

Player events are emitted by wrappers around the configured `MediaBuffer` and
`PlaybackDriver`, so segment fetching stays independent of application event
delivery. The event API has no external dependency and works in `no_std` builds
that provide `alloc`.

---

## Running the tests

```bash
# Run all unit + integration + doc tests
cargo test

# Run only ABR rule tests
cargo test --test abr_rules

# Run only media buffer abstraction tests
cargo test --test media_buffer

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
- `PlayerEvent::Log` messages and example-side Rust diagnostics.

**Prerequisites:** `wasm-pack` (`cargo install wasm-pack`)

```bash
# Build the WASM module (from workspace root)
wasm-pack build examples/wasm_player --target web --out-dir pkg

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

## Logging

dash-rs does not require a logging backend. Subscribe to `Player::on` and route
`PlayerEvent::Log` into your own logging system (`tracing`, `log`, browser UI,
or plain stdout).

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
└── MediaBuffer (trait) ←── NullBuffer            (headless / tests)
                         └── StubBuffer            (inspectable in-memory buffer)
                         └── BridgeMediaBuffer     (wasm_player — MSE via JS bridge)
                         └── GstMediaBuffer        (sink-gstreamer feature)
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
4. `MediaBuffer::append(mt, data, is_init)` forwards the bytes to the decoder.
5. At period boundaries, `fetch_next_segment` transparently advances to the next `Period` and re-fetches init segments before continuing.

---

## Spec compliance and missing features

For a full breakdown of DASH spec compliance, behavioural differences relative
to dash.js, and the prioritised list of unimplemented features, see
[/dash-compliance.md](docs/dash-compliance.md).
