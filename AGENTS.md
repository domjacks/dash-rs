# AGENTS.md

dash-rs — a Rust implementation of MPEG-DASH playback logic.
Pure Rust (edition 2021), async via `async-trait` (`?Send` everywhere);
WASM-compatible (`wasm32-unknown-unknown`).

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

# WASM player (requires wasm-pack)
wasm-pack build examples/wasm_player --target web --out-dir examples/wasm_player/pkg
# Then serve examples/wasm_player/ with any static file server, e.g.:
#   python3 -m http.server 8080
# and open http://localhost:8080

# Lint
cargo clippy -- -D warnings

# Format check
cargo fmt --check
```

### Prerequisites

No C toolchain or system library dependencies are required for the default
build.  All MPD parsing is handled by `quick-xml` + `serde` (pure Rust).

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

#[async_trait(?Send)]
impl SourceBuffer for MyBuffer { … }
```

Forgetting the macro, or using `#[async_trait]` (without `?Send`), produces
cryptic "associated type not found" errors.  All trait implementations must
use `?Send` because the library targets `wasm32-unknown-unknown` where most
types are not `Send`.

### `representations` must be sorted ascending by bandwidth

`AbrController::select`, `ThroughputRule`, `BufferOccupancyRule`, and
`InsufficientBufferRule` all assume index 0 is the lowest quality.
`DashSession::from_mpd` sorts them automatically via `extract_representations`.
If you build `RepresentationInfo` slices by hand (e.g. in tests), sort them
first.

### Audio-only and video-only streams are fully supported

`DashSession` sets `video_track = None` or `audio_track = None` when the
manifest has no corresponding `AdaptationSet`.  Use `session.has_video()` /
`session.has_audio()` to gate operations before calling
`fetch_next_segment`, `fetch_init_segment`, or `current_metrics` for that
track — calling them on an absent track returns
`DashError::NoRepresentations`.

### Multiple audio `AdaptationSet`s — highest-bandwidth wins, document order breaks ties

When a `Period` contains more than one audio `AdaptationSet` (e.g. a stream
offering both AAC-HE at low bitrates and AAC-LC at high bitrates, as in BBC
Radio One), `find_adaptation` selects the one whose representations have the
**highest maximum bandwidth**.  This ensures high-quality codec tiers are
preferred over lower-quality fallback tiers.  All representations within
the selected `AdaptationSet` must share the same codec (they form a single
`TrackState` with one MIME type).

When two `AdaptationSet`s have identical maximum bandwidth (e.g. BBC Radio
streams that offer main audio and an audio-description variant both at 320 kbps),
the one that appears **first in document order** is preferred.  This is
implemented by using `(bandwidth, usize::MAX - index)` as the composite key
in `max_by_key` inside `find_adaptation` (`src/session/mpd.rs`).

### `MediaSource::is_type_supported` — graceful track skip

`MediaSource` has a default method `is_type_supported(&self, mime_type: &str) -> bool`
that returns `true` for all types (preserving existing GStreamer / stub behaviour).

`from_mpd_with_utc` calls `is_type_supported` on the constructed MIME string
**before** calling `add_source_buffer`.  If it returns `false`:

- Video track: a `warn!` is emitted and `video_track` is set to `None`.
  The session continues as audio-only.
- Audio track: a `warn!` is emitted and `audio_track` is set to `None`.

The `BridgeMediaSource` in `examples/wasm_player/src/lib.rs` overrides this
method to delegate to `MediaSource.isTypeSupported()` in the browser, so HEVC
streams on Chrome/Firefox fall back to audio-only rather than crashing with a
`NotSupportedError` from `addSourceBuffer`.

Per-representation filtering (within a single `AdaptationSet` that mixes
supported and unsupported codecs) is tracked as P2-14 in `COMPLIANCE.md`.

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
- `period_start_s: f64` — `Period@start` in seconds (0.0 for the first/only period).
  Used by `seek_to` to map presentation-timeline positions back to segment numbers:
  `N = start_number + ⌊(position_s − period_start_s) / seg_dur_s⌋`.

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

After every **live manifest refresh** (`refresh_manifest`), both tracks are
re-snapped: if `segment_number` has fallen behind the new live edge (e.g. after
a long pause), it is advanced to `new_live_edge`.  `initial_segment_number` is
updated simultaneously so that `estimated_buffer_level_ms` counts from the new
position.

### `SegmentTimeline` r=-1 (infinite-repeat) walk

`find_timeline_segment` (`src/session/timeline.rs`) uses **arithmetic snapping**
for `r=-1` entries to avoid an infinite loop:

```
if (target_time − entry.t) % entry.d == 0 → match at target_time
```

An `r=-1` block is bounded by the next `S` entry's explicit `t` attribute
(when present).  Targets that fall at or beyond that `t` belong to the subsequent
entry and are not matched by the `r=-1` block.  This matches the BBC CMAF pattern
where the live window is represented as a single `<S t="…" d="…" r="-1"/>`.

### Live-edge `next_segment_time` snap for SegmentTimeline tracks

`snap_to_timeline_grid` (`src/session/timeline.rs`) is used during session
construction to set `TrackState::next_segment_time` for `$Time$`-addressed live
streams:

1. `compute_live_edge_segment` returns a segment number using the `$Number$` formula.
2. That number is converted to timescale ticks:
   `target_ticks = (initial_segment_number − start_number) × round(timescale × seg_dur_ms / 1000)`.
3. `snap_to_timeline_grid` snaps `target_ticks` **down** to the largest aligned
   `entry.t + k × entry.d ≤ target_ticks` within the SegmentTimeline.
4. The snapped value is stored as `next_segment_time` so the first
   `find_timeline_segment` call fetches the live-edge segment, not the beginning
   of the time-shift window.

`live_presentation_start_s()` returns `next_segment_time / timescale` for
SegmentTimeline tracks (when `next_segment_time` is set), so the WASM player's
`seekTo()` call targets the exact first-segment PTS rather than an approximation.
After `apply_live_pts_remap()` is called (see below), the method subtracts
`live_pts_remap_offset_s` and returns ≈ 0 for epoch-scale streams.

### `apply_live_pts_remap` — epoch-scale PTS remapping (WASM / MSE only)

On live streams where fMP4 decode timestamps are large absolute values
(e.g. BBC IPTV: segment 463 114 411 with `seg_dur = 3.84 s` → DTS start ≈
1 778 359 334 s), Safari resets `video.currentTime` to 0 before content is
buffered.  Seeking to 1.778 × 10⁹ s succeeds at the API level but the element
stalls because no data has yet been appended there.

Fix: call `session.apply_live_pts_remap()` (from `src/session/mod.rs`) **after**
session construction and **before** the segment fetch loop, for live streams only.

The method computes `offset_s`:

- `$Number$` tracks: `(initial_segment_number − start_number) × seg_dur_s`
- `$Time$` / SegmentTimeline tracks: `next_segment_time / timescale`

It then calls `sink.set_timestamp_offset(current − offset_s)` on every active
track sink and stores the result in `DashSession::live_pts_remap_offset_s`.
The first live-edge segment now lands at presentation time ≈ 0 in the browser.
`live_presentation_start_s()` subtracts `live_pts_remap_offset_s` and returns ≈ 0,
so `bridge.seekTo(0)` is a trivial seek that every browser handles.

**GStreamer clients must NOT call `apply_live_pts_remap()`**.  They use
`GstMediaSource::set_live_mode(true)` (probe-based PTS subtraction).
Double-applying would shift timestamps by twice the offset and corrupt decoding.

`seek_to` accounts for remapped streams: `position_s` is received in remapped
(≈0-based) presentation time.  The formula adds `live_pts_remap_offset_s` back
before computing the segment number, and subtracts it again from the returned
snapped position, so both the segment index and the caller's seek target are in
the correct coordinate space.  This is a no-op when `live_pts_remap_offset_s`
is 0.0 (VOD or non-remapped live streams).

`DashSession::live_pts_remap_offset_s()` exposes the stored offset for tests
and callers that need to convert between coordinate spaces.

`DashSession::buffered(media_type)` returns the current buffered time ranges
from the track's `SourceBufferSink`, delegating to the underlying
`SourceBuffer::buffered()`.  Used by the WASM player's `tick_gap` call to pass
real-time buffered ranges to the gap controller.  Returns an empty `Vec` when
the track is absent.

### `timestampOffset` formula (DASH spec §5.3.9.6)

`SourceBuffer.timestampOffset = Period@start − presentationTimeOffset/timescale`.

- `Period@start` is the presentation-timeline start of the period (0 for the
  first/only period; non-zero in multi-period VOD).
- `presentationTimeOffset` shifts decode timestamps within the period relative
  to `Period@start`.
- Together they ensure `DTS + timestampOffset` maps to the correct
  presentation-timeline position.

`compute_track_reinit` in `src/session/mod.rs` computes this correctly.
`seek_to` uses `period_start_s` (stored in `TrackState`) — not `timestampOffset`
directly — so that PTO does not affect the seek formula.

### `SegmentList@startNumber`

`extract_segment_list_timing` (`src/session/mpd.rs`) honours the
`@startNumber` attribute.  The segment-URL array index is computed as
`segment_number − start_number`, so a `SegmentList` with `startNumber="1"` works
identically to a `SegmentTemplate` with the same attribute.

### `MPD@minBufferTime` and `buffer_target_ms`

`DashSession::from_mpd_with_utc` seeds `buffer_target_ms` from `MPD@minBufferTime`
for VOD streams: `max(minBufferTime_ms, 12_000)`.  For live streams the default
12 000 ms is used regardless.  `DashSession::min_buffer_time_ms()` exposes the
parsed value.

### `should_refresh_manifest` when `minimumUpdatePeriod` absent

Per DASH spec §5.4, when `@minimumUpdatePeriod` is absent the client may refresh
as often as once per segment duration.  `should_refresh_manifest` therefore
falls back to the video (or audio) track's `segment_duration_ms` as the effective
update period.  Streams that omit this attribute will trigger a manifest refresh
before every segment fetch once the first segment duration has elapsed.

### Multi-period playback

`DashSession` transparently advances through all `Period` elements at period
boundaries.  When `fetch_next_segment` exhausts the current period's segments,
it calls `try_advance_period` internally: tracks are reinitialised from the new
period, init segments are re-fetched, and fetching resumes — all without the
caller seeing an `EndOfStream` error.  `EndOfStream` is only returned after the
last period is exhausted.

`period_count()` returns the total number of periods; `current_period_index()`
returns the zero-based index of the period currently being fetched.

### Segment URL resolution handles `SegmentTemplate` with `$Number$` and `$Time$`, `SegmentList`, and `SegmentBase`

`SegmentTemplate` addressing via `$Number$` (fixed-duration segments) and
`$Time$` (SegmentTimeline) are both supported.  `SegmentList` (explicit
`<SegmentURL>` elements) is also supported — `segment_urls[segment_number]`
is looked up on each fetch and the URL is resolved against the CDN base-URL
chain.  `SegmentBase` (byte-range addressing) is also supported — the `sidx`
index is fetched lazily on the first `fetch_next_segment` call using the
`indexRange` byte range, and each subsegment is then fetched as a byte-range
`GET` against the media file.  Byte-range `SegmentList` entries (`@mediaRange`)
and `SegmentBase` are not yet implemented.  `resolve_segment_url` returns
`DashError::SegmentUrlUnresolvable` for manifests that use those forms.

### `GapController` usage pattern

`GapController` is a caller-driven state machine — it has no internal timer
and performs no I/O.  The caller supplies the current renderer position and
buffered ranges; the controller returns a seek recommendation when needed.

`GapController` is **owned by `DashSession`**.  Callers do not instantiate it
directly; they use `DashSession::tick_gap`.  `DashSession::seek_to` calls
`GapController::on_seek` automatically, and the track-switch guard
(`on_track_switch_started/ended`) fires automatically at period boundaries
inside `try_advance_period`.

```rust
session.media_source_mut().set_playing()?;
session.notify_playing();

// In your fetch / UI loop (e.g. every 200 ms):
let target = session.tick_gap(
    renderer_position_s,    // from GStreamer query_position or equivalent
    &source_buffer_ranges,  // union of SourceBuffer::buffered() across tracks
    is_paused,
);
if let Some(t) = target {
    // 1. Seek the renderer.
    renderer.seek(t.position_s);
    // 2. Reposition the segment fetcher — also resets the gap controller.
    let snapped = session.seek_to(t.position_s);
    // 3. Restart the playback clock.
    session.notify_playing();
}
```

The **period-end stall jump** (dash.js: `timeToStreamEnd < smallGapLimit → seek
to stream end`) is not yet implemented in `GapController`.  If a live stream
stalls at a period boundary, call `session.seek_to(period_end_s)` manually.

### `GstSourceBuffer` / `GstMediaSource` known gaps and workarounds

- **Absolute-timestamp live streams (BBC CMAF)**: `DashSession::from_mpd_with_utc`
  snaps `next_segment_time` to the live-edge aligned position in the actual
  SegmentTimeline after computing the live-edge segment number (via
  `snap_to_timeline_grid` in `src/session/timeline.rs`).  This ensures the first
  fetch lands at the live edge rather than at the beginning of the time-shift
  window (which `find_timeline_segment(None)` would return).  `live_presentation_start_s()`
  returns the snapped tick value divided by the timescale for Timeline tracks, so
  the WASM player's `seekTo()` call is accurate.
  For GStreamer (`sink-gstreamer` feature), absolute-timestamp PTS remapping is
  handled by `GstMediaSource::set_live_mode(true)` (probe-based PTS subtraction) —
  call it after `DashSession::from_url` and before `set_playing` whenever
  `session.is_live()`.
- `buffered()` only tracks ranges from chunks whose `Chunk::start`/`end` are set.
  DASH segments currently do not carry timing (they are set to `None`), so the
  buffer-occupancy ABR rule sees an empty range.  Work-around: parse the fMP4
  `sidx` box to extract timing before building the `Chunk`.
- `remove()` does not flush GStreamer's downstream queue; it only removes the
  locally tracked time range.
- `change_type()` does not renegotiate caps with GStreamer at runtime.

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

## Documentation

- Ensure you keep the AGENTS.md, README.md and COMPLIANCE.md documents up to date whenever changes are made
- COMPLIANCE.md is used to track compliance against the DASH specs and Dash.js features. It should also be used to track behavioural differences where they occur along with their reason.

## Where to look

| What | Where |
|---|---|
| Public API surface | `src/lib.rs` |
| Error types | `src/error.rs` |
| Platform-agnostic monotonic clock helper | `src/time.rs` |
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
| Multi-period management (`try_advance_period`, `period_count`, `current_period_index`) | `src/session/mod.rs` |
| Epoch-scale PTS remapping (`apply_live_pts_remap`, `live_pts_remap_offset_s`) | `src/session/mod.rs` |
| MPD helpers (`merge_segment_templates`, `extract_segment_timing`, `find_adaptation`, etc.) | `src/session/mpd.rs` |
| SegmentTimeline / `$Time$` walking (`find_timeline_segment`, `snap_to_timeline_grid`) | `src/session/timeline.rs` |
| URL template expansion + BaseURL resolution | `src/session/segment_url.rs` |
| Live-edge segment calculation | `src/session/live.rs` |
| UTC clock sync (`resolve_utc_timing`, `query_http_utc`) | `src/session/utc.rs` |
| CDN blacklist (`CdnBlacklist`, service-location keying, cooldown) | `src/session/blacklist.rs` |
| Gap controller (stall detection, gap-jump logic) | `src/session/gap.rs` |
| XLink pre-processing (`resolve_xlinks`, Period + AdaptationSet) | `src/session/xlink.rs` |
| XS datetime parser (`parse_xs_datetime`) | `src/util.rs` |
| ISO BMFF box parsing (`sidx` timing, `sidx` byte-range index, `emsg`) | `src/mp4.rs` |
| ABR unit tests | `tests/abr_rules.rs` |
| MSE abstraction tests | `tests/media_source.rs` |
| Live-stream API + SegmentTimeline + UTCTiming + emsg tests | `tests/session.rs` |
| Stub end-to-end example | `examples/basic.rs` |
| Real HTTP fetch example | `examples/http_fetch.rs` |
| GStreamer full-playback example | `examples/gstreamer_player/main.rs` |
| WASM player example (`JsBridge`, `BridgeClock`, `BridgeFetcher`, `BridgeMediaSource`, `BridgeSourceBuffer`, `play`) | `examples/wasm_player/src/lib.rs` |
| WASM player MSE bridge (`createBridge`) — all browser Web API calls | `examples/wasm_player/player.js` |
| WASM player UI wiring — on-screen logger, WASM init, form handler | `examples/wasm_player/app.js` |

---

## dash.js mapping

| dash.js | dash-rs |
|---|---|
| `ExternalMediaSource.js` | `media_source::MediaSource` trait + `stub::StubMediaSource` |
| `ExternalSourceBuffer.js` | `media_source::SourceBuffer` trait + `stub::StubSourceBuffer` |
| `SourceBufferSink.js` | `media_source::sink::SourceBufferSink` |
| `FragmentLoader.js` / `URLLoader.js` | `fetch::SegmentFetcher` trait + `fetch::stub::StubSegmentFetcher` |
| `ManifestLoader.js` | `manifest::ManifestLoader` |
| `XlinkController.js` / `XlinkLoader.js` | `session::xlink::resolve_xlinks` (called automatically inside `DashSession::from_url`) |
| `ThroughputRule.js` | `abr::throughput::ThroughputRule` |
| `BolaRule.js` | `abr::buffer::BufferOccupancyRule` |
| `InsufficientBufferRule.js` | `abr::insufficient_buffer::InsufficientBufferRule` |
| `SwitchHistoryRule.js` | `abr::switch_history::SwitchHistoryRule` |
| `DroppedFramesRule.js` | `abr::dropped_frames::DroppedFramesRule` |
| `AbandonRequestsRule.js` | `abr::abandon_requests::AbandonRequestsRule` |
| `AbrController.js` | `abr::AbrController` |
| `ManifestUpdater.js` | `session::DashSession::refresh_manifest_if_due` (called automatically inside `fetch_next_segment`) |
| `LocationSelector.js` | `DashSession::location_urls()` + `manifest_url()`; fallback logic inside `refresh_manifest` |
| `MediaPlayer.js` + `StreamProcessor.js` | `session::DashSession` |
| `StreamController.js` / `Stream.js` | `session::DashSession::try_advance_period` (called automatically inside `fetch_next_segment`) |
| `ScheduleController.js` | `session::DashSession::estimated_buffer_level_ms` + `notify_playing` + `buffer_target_ms` |
| `EventController.js` | `session::DashEvent` + `DashSession::subscribe_events` (broadcast channel; events dispatched by `fetch_next_segment`) |
| `ServiceDescriptionController.js` | `DashSession::target_latency_ms()` + `playback_rate_range()` |
| `AbrController.setMaxAllowedBitrateFor()` | `DashSession::set_max_bitrate(media_type, bps)` / `clear_max_bitrate` / `max_bitrate` |
| `BlacklistController.js` | `DashSession::set_cdn_blacklist_cooldown_ms` / `cdn_blacklist_cooldown_ms`; blacklist logic in `src/session/blacklist.rs` |
| `GapController.js` | `session::GapController` (`src/session/gap.rs`); owned by `DashSession`; caller drives via `DashSession::tick_gap(position_s, buffered, is_paused)`; `DashSession::seek_to` automatically resets the controller via `on_seek()`; track-switch guard fires automatically at period boundaries |

---

## Spec compliance and missing features

For a full breakdown of DASH spec compliance, implemented items, behavioural
differences relative to dash.js, and the prioritised list of unimplemented
features, see [COMPLIANCE.md](COMPLIANCE.md).
