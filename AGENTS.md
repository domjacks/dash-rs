# AGENTS.md

dash-rs — a Rust implementation of MPEG-DASH playback logic.
Pure Rust (edition 2021), async via `tokio` / `async-trait`.

---

## Build & test

```bash
# Compile (requires protoc on PATH — see Prerequisites below)
cargo build

# All tests: unit, integration, doc-tests
cargo test

# A single integration suite
cargo test --test abr_rules
cargo test --test media_source

# Doc-tests only
cargo test --doc

# Stub end-to-end example (no network required)
cargo run --example basic

# Real HTTP fetch example (requires internet access)
cargo run --example http_fetch
# Override the manifest URL
cargo run --example http_fetch -- https://your-stream/manifest.mpd

# GStreamer playback example (requires sink-gstreamer feature + GStreamer installed)
cargo run --features sink-gstreamer --example gstreamer_player
cargo run --features sink-gstreamer --example gstreamer_player -- https://your-stream/manifest.mpd

# Lint
cargo clippy -- -D warnings

# Format check
cargo fmt --check
```

### Prerequisites

`dash-mpd` pulls in `pssh-box`, which generates Rust from `.proto` files at
build time.  `protoc` must be on `PATH`:

```bash
# macOS
brew install protobuf

# Debian / Ubuntu
apt-get install protobuf-compiler

# Windows — download from https://github.com/protocolbuffers/protobuf/releases
```

#### `sink-gstreamer` feature

Requires GStreamer ≥ 1.14 headers.  The Homebrew formula bundles all required
plugins in a single install:

```bash
brew install gstreamer       # macOS
```

If `cargo build --features sink-gstreamer` cannot find the headers, prepend
the framework `bin` directory so its own `pkg-config` is found first:

```bash
export PATH="/Library/Frameworks/GStreamer.framework/Versions/1.0/bin:$PATH"
```

---

## Architecture gotchas

### Async traits use `async-trait`

All `async fn` methods on `SourceBuffer`, `MediaSource`, and `SegmentFetcher`
are defined with the `#[async_trait]` macro.  Implementations must import and
apply it:

```rust
use async_trait::async_trait;

#[async_trait]
impl SourceBuffer for MyBuffer { … }
```

Forgetting the macro produces cryptic "associated type not found" errors.

### `representations` must be sorted ascending by bandwidth

`AbrController::select`, `ThroughputRule`, `BufferOccupancyRule`, and
`InsufficientBufferRule` all assume index 0 is the lowest quality.
`DashSession::from_mpd` sorts them automatically via `extract_representations`.
If you build `RepresentationInfo` slices by hand (e.g. in tests), sort them
first.

### `AbrController::select` takes `&self`; `record_switch` takes `&mut self`

`select` is intentionally immutable so it can be called while other state is
mutably borrowed.  Call `record_switch` separately after a quality change has
actually been applied to track state.  See `DashSession::fetch_next_segment`
for the correct pattern.

### `StubSourceBuffer` only tracks timed chunks in `buffered()`

Init segments (`Chunk { start: None, end: None }`) are accepted without error
but are not reflected in `buffered()`.  This matches the MSE spec — init
segments do not advance the buffered range.

### `SourceBufferSink::append` serialises by `await`

There is no explicit queue struct.  Concurrent callers must not interleave
`await` points across two `append` calls on the same sink.  Use a single
`async` task or a mutex if multiple producers exist.

### `DashSession` buffer scheduling API

`DashSession` exposes a dash.js `ScheduleController`-style buffer-level gate:

| Method | Description |
|---|---|
| `notify_playing()` | Call immediately after `media_source.set_playing()`. Starts the wall-clock used by `estimated_buffer_level_ms`. |
| `estimated_buffer_level_ms(media_type)` | Returns `(segments_fetched × segment_duration_ms) − elapsed_playing_ms`, clamped to 0. |
| `buffer_target_ms()` | Default 12 000 ms (matches `bufferTimeDefault` in dash.js `ScheduleController`). |
| `set_buffer_target_ms(ms)` | Override the target. |

The fetch loop pattern mirrors dash.js:

```rust
session.media_source_mut().set_playing()?;
session.notify_playing();

loop {
    while session.estimated_buffer_level_ms(&MediaType::Video) >= session.buffer_target_ms() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    session.fetch_next_segment(MediaType::Video, &metrics).await?;
}
```

`estimated_buffer_level_ms` is a wall-clock approximation — it assumes real-time
playback speed.  It is meaningless before `notify_playing()` is called (returns 0).
`segment_duration_ms` is extracted from `SegmentTemplate.duration / timescale × 1000`
at session construction time (`extract_segment_timing()` in `src/session.rs`).

### `TrackState` segment timing fields

`TrackState` carries two timing fields populated from the MPD:

- `start_number: u64` — `SegmentTemplate@startNumber` (default 1).
- `segment_duration_ms: f64` — `SegmentTemplate.duration / timescale × 1000` (default 2 000 ms if absent).

`segments_fetched` is computed as `segment_number − start_number` (not a separate counter).
`segment_number` advances by 1 on every successful `fetch_next_segment` call.

### `DashSession` uses the first `Period` only

Multi-period support is not yet implemented.  Passing an MPD with multiple
periods will not error, but only the first period's tracks are used.

### Segment URL resolution only handles `SegmentTemplate` with `$Number$`

`$Time$` addressing, `SegmentList`, and byte-range `SegmentBase` are not
yet implemented.  `resolve_segment_url` returns `DashError::SegmentUrlUnresolvable`
for manifests that use those forms.

---

## Code conventions

### Error handling

All public functions return `Result<_, DashError>`.  Add new variants to
`src/error.rs`; never use `unwrap` or `expect` in library code.

### Logging

Use `tracing` macros — never `println!` or `eprintln!` in library code.

| Level | When to use |
|---|---|
| `tracing::error!` | Unrecoverable conditions that will propagate as errors |
| `tracing::warn!` | Recoverable conditions the caller should know about (e.g. buffer critically low) |
| `tracing::info!` | State transitions (quality switch, end of stream, manifest loaded) |
| `tracing::debug!` | Per-operation details (every append, every fetch, every ABR decision) |

### ABR rules

Each rule lives in its own file under `src/abr/`.  Rules must:

- Implement `AbrRule` (in `src/abr/rule.rs`).
- Return `None` to abstain (not `Some(usize::MAX)`).
- Be stateless where possible.  If state is needed (e.g. `SwitchHistoryRule`),
  keep it inside the struct and expose a `record_*` method called by
  `AbrController`.
- Be registered in `AbrController::default()` in `src/abr/mod.rs`.
- Have unit tests in `tests/abr_rules.rs`.

### Adding a `MediaSource` or `SourceBuffer` implementation

Implement the trait in a new module (e.g. `src/media_source/gstreamer.rs`).
Do not modify `src/media_source/mod.rs` (the trait definitions) unless the
MSE spec requires a new method.

### Adding a `SegmentFetcher` implementation

Implement the trait in a new module under `src/fetch/` (e.g.
`src/fetch/http.rs`).  Gate it behind a Cargo feature if it adds heavy
dependencies (e.g. `reqwest`):

```toml
[features]
fetch-http = ["dep:reqwest", "dep:tokio"]
```

---

## Where to look

| What | Where |
|---|---|
| Public API surface | `src/lib.rs` |
| Error types | `src/error.rs` |
| ABR rule trait | `src/abr/rule.rs` |
| ABR controller | `src/abr/mod.rs` |
| `MediaSource` / `SourceBuffer` traits | `src/media_source/mod.rs` |
| Append queue + window management | `src/media_source/sink.rs` |
| In-memory stub implementations | `src/media_source/stub.rs` |
| GStreamer `MediaSource` / `SourceBuffer` (`sink-gstreamer` feature) | `src/media_source/gstreamer.rs` |
| `SegmentFetcher` trait | `src/fetch/mod.rs` |
| No-op stub fetcher | `src/fetch/stub.rs` |
| Manifest loading | `src/manifest/mod.rs` |
| Session coordinator | `src/session.rs` |
| Segment timing extraction | `extract_segment_timing()` in `src/session.rs` |
| ABR unit tests | `tests/abr_rules.rs` |
| MSE abstraction tests | `tests/media_source.rs` |
| Stub end-to-end example | `examples/basic.rs` |
| Real HTTP fetch example | `examples/http_fetch.rs` |
| GStreamer full-playback example | `examples/gstreamer_player.rs` |

---

## dash.js mapping

| dash.js | dash-rs |
|---|---|
| `ExternalMediaSource.js` | `media_source::MediaSource` trait + `stub::StubMediaSource` |
| `ExternalSourceBuffer.js` | `media_source::SourceBuffer` trait + `stub::StubSourceBuffer` |
| `SourceBufferSink.js` | `media_source::sink::SourceBufferSink` |
| `FragmentLoader.js` / `URLLoader.js` | `fetch::SegmentFetcher` trait + `fetch::stub::StubSegmentFetcher` |
| `ManifestLoader.js` | `manifest::ManifestLoader` |
| `ThroughputRule.js` | `abr::throughput::ThroughputRule` |
| `BolaRule.js` | `abr::buffer::BufferOccupancyRule` |
| `InsufficientBufferRule.js` | `abr::insufficient_buffer::InsufficientBufferRule` |
| `SwitchHistoryRule.js` | `abr::switch_history::SwitchHistoryRule` |
| `AbrController.js` | `abr::AbrController` |
| `MediaPlayer.js` + `StreamProcessor.js` | `session::DashSession` |
| `ScheduleController.js` | `session::DashSession::estimated_buffer_level_ms` + `notify_playing` + `buffer_target_ms` |

---

## Not yet implemented

The following features from dash.js have no Rust equivalent yet.  PRs welcome.

- Live / dynamic MPD refresh (`ManifestUpdater`).
- `SegmentList` and `SegmentBase` (byte-range) segment addressing.
- `$Time$` template variable in `SegmentTemplate`.
- Multi-period playback.
- Subtitle / text track handling.
- DRM / Encrypted Media Extensions.
- MPD patch (`<Patch>` element).

### `GstSourceBuffer` known gaps

- `buffered()` only tracks ranges from chunks whose `Chunk::start`/`end` are set.
  DASH segments currently do not carry timing (they're set to `None`), so the
  buffer-occupancy ABR rule sees an empty range.  Work-around: parse the fMP4
  `sidx` box to extract timing before building the `Chunk`.
- `remove()` does not flush GStreamer's downstream queue; it only removes the
  locally tracked time range.
- `change_type()` does not renegotiate caps with GStreamer at runtime.
