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
at session construction time (`extract_segment_timing()` in `src/session/mpd.rs`).

### `TrackState` segment timing fields

`TrackState` carries these timing fields populated from the MPD:

- `start_number: u64` — `SegmentTemplate@startNumber` (default 1).
- `segment_duration_ms: f64` — `SegmentTemplate.duration / timescale × 1000` (default 2 000 ms if absent).
- `initial_segment_number: u64` — segment number at which *this session* started fetching.
  For VOD this equals `start_number`.  For live streams it is set to the live-edge
  segment after snapping so that `estimated_buffer_level_ms` counts only segments
  pushed in this session, not from the beginning of the broadcast.

`segments_fetched_for_eos` is computed as `segment_number − start_number` (used by the
VOD EOS segment-count check in `fetch_next_segment`; live streams use `total_segments = None`
so this path is skipped).

`segments_fetched_for_buffer` is computed as `segment_number − initial_segment_number` (used
by `estimated_buffer_level_ms`).

`segment_number` advances by 1 on every successful `fetch_next_segment` call.

For **live streams** (`MPD@type="dynamic"`), `from_mpd` / `from_url` snap
both `segment_number` and `initial_segment_number` to the live edge on construction using
`availabilityStartTime`:

```
segment_number = floor((now − availabilityStartTime) / seg_dur) + startNumber − safety_margin
initial_segment_number = segment_number   (set at the same time)
```

`safety_margin` defaults to 3 segments; `MPD@suggestedPresentationDelay`
overrides it when present.  `segment_number` is clamped to at least
`start_number`.  The helper is `compute_live_edge_segment()` in
`src/session/live.rs`.

### `DashSession` uses the first `Period` only

Multi-period support is not yet implemented.  Passing an MPD with multiple
periods will not error, but only the first period's tracks are used.

### Segment URL resolution handles `SegmentTemplate` with `$Number$` and `$Time$`, and `SegmentList`

`SegmentTemplate` addressing via `$Number$` (fixed-duration segments) and
`$Time$` (SegmentTimeline) are both supported.  `SegmentList` (explicit
`<SegmentURL>` elements) is also supported — `segment_urls[segment_number]`
is looked up on each fetch and the URL is resolved against the CDN base-URL
chain.  Byte-range `SegmentList` entries (`@mediaRange`) and `SegmentBase`
are not yet implemented.  `resolve_segment_url` returns
`DashError::SegmentUrlUnresolvable` for manifests that use those forms.

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

Concrete implementations that are only needed by a specific example belong
**alongside that example**, not in `src/`.  Create an
`examples/<example-name>/` subdirectory, put the implementation in its own
file (e.g. `examples/gstreamer_player/gst_media_source.rs`), and declare it
as a module from `main.rs` (`mod gst_media_source;`).  Import the library
traits via `dash_rs::media_source::{MediaSource, SourceBuffer, …}`.

Only promote an implementation into `src/` if it is intended to be reusable
by downstream crates.

Do not modify `src/media_source/mod.rs` (the trait definitions) unless the
MSE spec requires a new method.

### Adding a `SegmentFetcher` implementation

Concrete fetcher implementations that are only needed by a specific example
belong **alongside that example** (see `examples/http_fetch.rs` and
`examples/gstreamer_player/main.rs` for the established pattern).

Only promote a fetcher into `src/fetch/` if it is intended to be reusable by
downstream crates.  Gate it behind a Cargo feature if it adds heavy
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
| GStreamer `MediaSource` / `SourceBuffer` (`sink-gstreamer` feature) | `examples/gstreamer_player/gst_media_source.rs` |
| `SegmentFetcher` trait | `src/fetch/mod.rs` |
| No-op stub fetcher | `src/fetch/stub.rs` |
| Manifest loading | `src/manifest/mod.rs` |
| Session coordinator (`DashSession`, `TrackState`, `MediaType`, `DashEvent`) | `src/session/mod.rs` |
| MPD helpers (`merge_segment_templates`, `extract_segment_timing`, etc.) | `src/session/mpd.rs` |
| SegmentTimeline / `$Time$` walking | `src/session/timeline.rs` |
| URL template expansion + BaseURL resolution | `src/session/segment_url.rs` |
| Live-edge segment calculation | `src/session/live.rs` |
| UTC clock sync (`resolve_utc_timing`, `query_http_utc`) | `src/session/utc.rs` |
| XS datetime parser (`parse_xs_datetime`) | `src/util.rs` |
| ISO BMFF box parsing (`sidx`, `emsg`) | `src/mp4.rs` |
| ABR unit tests | `tests/abr_rules.rs` |
| MSE abstraction tests | `tests/media_source.rs` |
| Live-stream API + SegmentTimeline + UTCTiming + emsg tests | `tests/session.rs` |
| Stub end-to-end example | `examples/basic.rs` |
| Real HTTP fetch example | `examples/http_fetch.rs` |
| GStreamer full-playback example | `examples/gstreamer_player/main.rs` |

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
| `DroppedFramesRule.js` | `abr::dropped_frames::DroppedFramesRule` |
| `AbandonRequestsRule.js` | `abr::abandon_requests::AbandonRequestsRule` |
| `AbrController.js` | `abr::AbrController` |
| `ManifestUpdater.js` | `session::DashSession::refresh_manifest_if_due` (called automatically inside `fetch_next_segment`) |
| `MediaPlayer.js` + `StreamProcessor.js` | `session::DashSession` |
| `ScheduleController.js` | `session::DashSession::estimated_buffer_level_ms` + `notify_playing` + `buffer_target_ms` |
| `EventController.js` | `session::DashEvent` + `DashSession::subscribe_events` (broadcast channel; events dispatched by `fetch_next_segment`) |

---

## Not yet implemented

The following features from dash.js have no Rust equivalent yet.  Grouped by
implementation priority.  PRs welcome.

### Tier 1 — Core stream compatibility (blocks many real-world streams)

| Feature | dash.js equivalent | Implementation notes |
|---|---|---|
| SegmentBase (byte-range) | `SegmentBaseGetter.js`, `SegmentBaseLoader.js` | `FetchRequest::byte_range` field exists but is never set. Fetch the `sidx`/index, walk byte ranges, set the field. |
| Multi-period playback | `StreamController.js` / `Stream.js` | `from_mpd` and `fetch_next_segment` currently hardcode `periods[0]`. Need period switching, `Period@duration`-based EOS detection, and `TrackState` re-initialisation on period boundary. |
| XLink resolution | `XlinkLoader.js` / `XlinkController.js` | `xlink:href` on `Period`, `AdaptationSet`, etc. are ignored by `dash-mpd`. Add a pre-processing pass that fetches and inlines remote elements before session construction. |

### Tier 2 — Important for live streams and ABR quality

| Feature | dash.js equivalent | Implementation notes |
|---|---|---|
| Content steering server | `ContentSteeringController.js` | Static CDN failover exists. Parse `<ContentSteering>` from the MPD; spawn a refresh task that polls the steering server and updates the ordered base-URL list in `DashSession`. |
| L2ARule (low-latency Lyapunov ABR) | `L2ARule.js` | New file `src/abr/l2a.rs`. Implements the Lyapunov optimisation tuned for low-latency streams. Register in `AbrController::default()`. |
| LoLpRule (learning-based low-latency ABR) | `LoLpRule.js` / `LearningAbrController.js` | New file `src/abr/lolp.rs`. Requires a QoE evaluator and an online weight-selector; more complex than other rules. |
| Low-latency partial/chunked segments | `PartialSegment.js`, `FetchLoader.js` | Requires `SegmentFetcher` to support progressive byte delivery (streaming body). `FetchResponse` currently holds complete `Bytes`; need a `Stream<Item=Bytes>` variant or callback. |
| `<ServiceDescription>` | `ServiceDescriptionController.js` | Parse target latency and playback-rate bounds. Expose `target_latency_ms()` and `playback_rate_range()` on `DashSession`. |
| ProducerReferenceTime (PRFT) | `ProducerReferenceTime.js` | Parse the `prft` box from fMP4 segments to measure producer-to-consumer latency. Expose the measurement alongside `AbrMetrics`. |
| Catch-up playback rate control | `CatchupController.js` | Add `set_playback_rate(f64)` to the `MediaSource` trait. `DashSession` computes the catch-up rate from `target_latency − actual_latency` and calls it. |
| MPD Patch (`<Patch>`) | `PatchManifestModel.js`, `ManifestUpdater.js` | Parse `PatchLocation` from the manifest. In `refresh_manifest_if_due`, if a patch URL is present, fetch and apply the XPath patch operations instead of re-fetching the full MPD. |

### Tier 3 — Metrics, DRM, text, thumbnails

| Feature | dash.js equivalent | Implementation notes |
|---|---|---|
| Formal metrics collection | `DashMetrics.js` / `MetricsModel.js` | Add a `DashMetrics` struct that `DashSession` populates on each fetch/append: buffer level samples, HTTP request records, representation-switch events. Expose via `session.metrics()`. |
| DVB/ISO metrics reporting | `MetricsReporting.js` / `DVBReporting` | After `DashMetrics` exists, add an optional HTTP reporter that POSTs samples to a `@dvb:metrics` service location from the MPD. Gate behind a Cargo feature. |
| CMCD / CMSD | `CmcdModel.js` / `CmsdModel.js` | Build CMCD query parameters from `AbrMetrics` + session state and attach to each `FetchRequest`. Parse `CMSD` response headers from `FetchResponse`. Gate behind a feature. |
| Subtitle / text tracks | `TextController.js` / `TTMLParser.js` / `VTTParser.js` | Add `MediaType::Text`. In `from_mpd`, find text `AdaptationSet`s and create a third `TrackState`. Need a `TextParser` trait with TTML and VTT implementations. |
| Thumbnails / trick play | `ThumbnailController.js` / `ThumbnailTracks.js` | Parse thumbnail `AdaptationSet` (role=`thumbnail`). Add `DashSession::thumbnail_at(time_s)` that returns a sprite-sheet tile URL and crop rect. |
| DRM / Content Protection | `streaming/protection/` | Add a `ContentProtection` descriptor to `RepresentationInfo`. Expose a `DrmKeyProvider` trait. `DashSession` calls it when a `pssh` box is encountered. Platform-specific EME/CDM integration belongs in examples. |
| Representation bitrate cap API | `AbrController.setMaxAllowedBitrateFor()` | Add `DashSession::set_max_bitrate(media_type, bps)`. Inject a capping rule into `AbrController` that enforces it. |
| Codec capability filtering | `Capabilities.js` / `CapabilitiesFilter.js` | Add a `CodecCapabilities` trait. `DashSession::from_mpd` calls `capabilities.is_supported(mime)` and filters representations before `TrackState` is built. GStreamer implementation belongs in the example. |
| Offline download | `src/offline/` | Native equivalent: a download coordinator that writes segments to a directory with a manifest index. Out of scope until the above tiers are complete. |

### `GstSourceBuffer` / `GstMediaSource` known gaps and workarounds

- **Absolute-timestamp live streams (BBC CMAF)**: handled by
  `GstMediaSource::set_live_mode(true)`, which installs a combined
  `NEWSEGMENT` + `BUFFER` pad probe on each converter's sink pad.  The probe
  replaces every qtdemux `NEWSEGMENT` event with a zeroed TIME segment
  `{start=0, time=0, base=0}` and subtracts the first absolute PTS seen from
  every decoded buffer's PTS and DTS.  This remaps Unix-epoch timestamps
  (≈ 1.778 × 10¹⁸ ns for BBC Two HD) to relative values starting at zero so
  that GStreamer's normal `sync=true` frame-pacing works correctly.  Call
  `set_live_mode(true)` after `DashSession::from_url` and before `set_playing`
  whenever `session.is_live()`.
- `buffered()` only tracks ranges from chunks whose `Chunk::start`/`end` are set.
  DASH segments currently do not carry timing (they're set to `None`), so the
  buffer-occupancy ABR rule sees an empty range.  Work-around: parse the fMP4
  `sidx` box to extract timing before building the `Chunk`.
- `remove()` does not flush GStreamer's downstream queue; it only removes the
  locally tracked time range.
- `change_type()` does not renegotiate caps with GStreamer at runtime.
