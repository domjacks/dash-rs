# Architecture

`dash-rs` is split between a reusable DASH playback engine and small
application-provided adapters for networking and rendering.

The normal application entry point is `Player`. Lower-level code can still use
`DashSession` directly when it needs a custom fetch loop.

```text
Application
├── SegmentFetcher implementation  (HTTP, browser fetch, test stub)
├── MediaBuffer implementation     (MSE, GStreamer, headless test sink)
└── PlaybackDriver implementation  (renderer state and renderer actions)

Player
├── DashSession
├── MediaBuffer
├── PlaybackDriver
└── PlayerEvent callbacks

DashSession
├── ManifestLoader
│   └── manifest::parse            (roxmltree MPD parser)
├── AbrController
│   ├── ThroughputRule
│   ├── BufferOccupancyRule
│   ├── InsufficientBufferRule
│   ├── SwitchHistoryRule
│   ├── DroppedFramesRule
│   └── AbandonRequestsRule
├── GapController
├── DashMetrics
├── CdnBlacklist
└── TrackState                     (one each for video/audio when present)
```

## Public Integration Surface

| Type | Location | Role |
|---|---|---|
| `Player` | `src/player.rs` | Owns `DashSession`, `MediaBuffer`, and `PlaybackDriver`; emits `PlayerEvent`s |
| `SegmentFetcher` | `src/fetch/mod.rs` | Fetches manifests, media segments, byte ranges, and UTC timing endpoints |
| `MediaBuffer` | `src/media_buffer.rs` | Receives initialization/media bytes and maps timeline state into the renderer |
| `PlaybackDriver` | `src/session/driver.rs` | Reports renderer position/state and handles renderer seeks or lifecycle hooks |
| `Clock` | `src/clock.rs` | Supplies monotonic time, UTC time, and async sleeps for native, WASM, and no-std targets |
| `PlayerSettings` | `src/settings.rs` | Construction-time tuning (buffer target, ABR thresholds, live safety margin, gaps, CDN cooldown) |

Built-in buffers are intentionally minimal:

| Buffer | Role |
|---|---|
| `NullBuffer` | Discards all data for headless tests/examples |
| `StubBuffer` | Records appended chunks for assertions |
| `BridgeMediaBuffer` | Browser MSE integration in `examples/wasm_player` |
| `GstMediaBuffer` | GStreamer integration in `examples/gstreamer_player` |

## Manifest And Session Construction

`Player::from_url` delegates to `DashSession::from_url`.

```text
URL
└── ManifestLoader::load
    ├── SegmentFetcher::fetch(RequestType::Manifest)
    ├── response.final_url becomes manifest base URL
    └── manifest::parse(xml)
        └── roxmltree tree walk into manifest::MPD

DashSession::from_url_with_settings   (from_url / from_url_with_clock use PlayerSettings::default())
├── resolve eager XLink references
├── resolve UTCTiming to compute server/client clock offset
└── from_mpd_with_utc
    ├── choose video/audio AdaptationSet
    ├── extract sorted RepresentationInfo lists
    ├── compute MIME, timestampOffset, append window, and segment timing
    ├── create TrackState for available tracks
    ├── snap live streams to the live edge
    └── initialize buffer target, ABR controller, metrics, gap controller, and CDN blacklist from PlayerSettings
```

The MPD model in `src/manifest/mpd_types.rs` intentionally declares only the
fields the playback engine currently consumes. Unknown XML elements and
attributes are ignored by the parser.

## Playback Loop

`Player::run()` either delegates directly to `DashSession::run()` or wraps the
buffer and driver so it can emit `PlayerEvent`s.

Startup sequence:

1. `MediaBuffer::open()`.
2. `MediaBuffer::set_live_mode(is_live)`.
3. `MediaBuffer::set_duration(INFINITY)` for live streams.
4. `MediaBuffer::add_track()` for video/audio; rejected tracks are disabled.
5. `set_timestamp_offset()` and `set_append_window()` for active tracks.
6. Fetch and append initialization segments.
7. Optionally apply live PTS remapping when `PlaybackDriver::wants_pts_remap()`.
8. Call `PlaybackDriver::on_session_started(start_s)`.
9. Start the session playback clock.

Per-iteration sequence:

1. Exit if all tracks reached EOS or `should_continue()` returns `false`.
2. Consume `pending_seek_s()` and reposition the fetch cursor when needed.
3. Sync `PlaybackState` from the driver.
4. Call `on_iteration_start()` with estimated position and buffer levels.
5. Optionally prune old buffer ranges.
6. Tick the gap controller and call `on_gap_seek()` if a seek is recommended.
7. Wait for buffer capacity.
8. Build `AbrMetrics`, select quality, fetch the next video segment, and append it.
9. Apply the audio gate target, then fetch and append the next audio segment.
10. Emit `on_quality_change()` and `on_iteration()` callbacks.
11. After loop exit, call `MediaBuffer::end_of_stream()`.

## Segment Fetch Flow

```text
AbrMetrics
└── AbrController::select
    └── QualityDecision(index, reason)
        └── TrackState.current_index
            └── resolve segment request
                ├── SegmentTemplate $Number$
                ├── SegmentTemplate $Time$ + SegmentTimeline
                ├── SegmentList
                └── SegmentBase + sidx
                    └── BaseURL candidate list
                        ├── priority/weight ordering
                        ├── CDN blacklist skip/retry
                        └── SegmentFetcher::fetch
                            └── MediaBuffer::append
```

On successful segment fetches, the session records HTTP metrics, throughput,
buffer samples, and representation switches in `DashMetrics`.

Live streams also check:

- `minimumUpdatePeriod` for manifest refresh.
- `availabilityEndTime` for clean broadcast shutdown.
- `timeShiftBufferDepth` to avoid fetching expired DVR-window segments.
- HTTP 404 on not-yet-generated segments, with a one-segment retry delay.

## Periods, Seeking, And Timing

`DashSession` stores one `TrackState` per active media type. Each track tracks
its current representation, current segment number, segment duration, addressing
mode, period start, timestamp offset, append window, and live-start segment.

At period boundaries, `fetch_next_segment` calls the internal period-advance
path. The new period recomputes track state, updates append windows and codec
type, fetches any required init segments, and continues without returning EOS
to the caller. EOS is returned only after the final period is exhausted.

`seek_to(position_s)` snaps the fetch cursor to the corresponding segment
boundary, resets gap state, and accounts for live PTS remapping when enabled.
During `run()`, user seeks should flow through `PlaybackDriver::pending_seek_s()`.

Live timestamp handling has two platform paths:

| Platform | Path |
|---|---|
| Browser MSE | `PlaybackDriver::wants_pts_remap() == true`; session shifts timestamp offsets so live-edge media starts near `0` |
| GStreamer | `wants_pts_remap() == false`; the media buffer handles live timestamp subtraction via `set_live_mode(true)` |

## Events

`Player::on()` registers callbacks for `PlayerEvent`.

Events are emitted by lightweight wrappers around the configured
`MediaBuffer` and `PlaybackDriver`:

- `EventMediaBuffer` forwards every buffer call and extracts in-band DASH
  `emsg` boxes from media segments.
- `EventPlaybackDriver` forwards every driver call and emits playback-state,
  gap-seek, quality-change, iteration, and session-start events.

If no event handlers are registered, `Player::run()` calls `DashSession::run()`
directly and avoids the wrappers.

## Module Map

| Path | Responsibility |
|---|---|
| `src/lib.rs` | Public module exports and top-level re-exports |
| `src/player.rs` | `Player`, `PlayerEvent`, event wrapper adapters |
| `src/settings.rs` | `PlayerSettings` and its sub-structs with dash.js defaults |
| `src/session/mod.rs` | Main DASH session coordinator and fetch loop |
| `src/session/mpd.rs` | MPD-to-track extraction helpers |
| `src/session/segment_url.rs` | Template expansion and BaseURL candidate resolution |
| `src/session/timeline.rs` | SegmentTimeline lookup and live-grid snapping |
| `src/session/live.rs` | Live-edge segment calculation |
| `src/session/gap.rs` | Gap and stall detection |
| `src/session/utc.rs` | UTCTiming resolution |
| `src/session/xlink.rs` | Eager XLink manifest expansion |
| `src/session/blacklist.rs` | Temporary CDN failure blacklist |
| `src/session/metrics.rs` | DASH metric records |
| `src/manifest/` | MPD parser and MPD data model |
| `src/abr/` | ABR controller and ABR rules |
| `src/fetch/` | Fetch request/response traits and stub fetcher |
| `src/media_buffer.rs` | Media sink trait and test/headless buffers |
| `src/mp4.rs` | Minimal MP4 helpers for `sidx`, `emsg`, and `prft` |
| `src/clock.rs` | Platform clock abstraction |

## Examples And Tests

| Path | Purpose |
|---|---|
| `examples/basic.rs` | Offline stub example |
| `examples/http_fetch.rs` | Real HTTP fetcher with `NullBuffer` |
| `examples/wasm_player/` | Browser MSE playback through a JS bridge |
| `examples/gstreamer_player/` | Native playback with GStreamer |
| `tests/abr_rules.rs` | ABR rule behavior |
| `tests/media_buffer.rs` | Media buffer behavior |
| `tests/session.rs` | Session construction, playback, seeking, live, and period behavior |
