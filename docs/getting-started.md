# Getting started

`dash-rs` provides DASH playback logic while your application provides the
platform-specific networking and renderer integration. The recommended entry
point is `Player`, which owns the playback session, media buffer, and playback
driver together.

To integrate the library, implement three traits:

| Trait | Role |
|---|---|
| `SegmentFetcher` | Downloads manifests and media segments |
| `MediaBuffer` | Connects fetched media bytes to your decoder or media pipeline |
| `PlaybackDriver` | Reports renderer state and handles renderer actions |

`Player::run()` then handles manifest-driven playback, including startup, ABR,
buffer scheduling, live refreshes, multi-period transitions, and end-of-stream
signalling.

## 1. Add the dependency

```toml
[dependencies]
dash-rs     = { git = "https://github.com/jacksd60/dash-rs" }
async-trait = "0.1"
bytes       = "1"
reqwest     = "0.13"
tokio       = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The examples below use `reqwest` and `tokio`, but the library does not require
either. You can use any async executor and networking stack.

## 2. Implement `SegmentFetcher`

`SegmentFetcher` is the networking adapter. Every manifest and media request
passes through it.

```rust
use std::time::Instant;

use async_trait::async_trait;
use dash_rs::{FetchError, FetchRequest, FetchResponse, SegmentFetcher, Url};

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

        let t0 = Instant::now();
        let resp = builder
            .send()
            .await
            .map_err(|e| FetchError::Network {
                url: req.url.clone(),
                message: e.to_string(),
            })?;
        let status = resp.status().as_u16();
        let final_url = dash_rs::Url::parse(resp.url().as_str())
            .unwrap_or_else(|_| req.url.clone());
        if !resp.status().is_success() {
            return Err(FetchError::Http {
                status,
                url: req.url,
            });
        }
        let data = resp
            .bytes()
            .await
            .map_err(|e| FetchError::Network {
                url: final_url.clone(),
                message: e.to_string(),
            })?;

        Ok(FetchResponse {
            final_url,
            transfer_duration_ms: t0.elapsed().as_millis() as u64,
            bytes_received: data.len() as u64,
            data,
        })
    }

    async fn check_existence(&self, url: &Url) -> Result<bool, FetchError> {
        let resp = self
            .client
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
```

For live streams that use HTTP `Date` headers for UTC synchronization, also
override `SegmentFetcher::head_date`.

## 3. Implement `MediaBuffer`

`MediaBuffer` connects downloaded bytes to your platform's media pipeline.
Implement `append` to deliver initialization and media segments. Override the
other methods your platform needs:

| Method | When to override |
|---|---|
| `open()` | Create or initialize the underlying media pipeline |
| `add_track()` | Allocate a decoder track or browser `SourceBuffer`; return `false` for unsupported codecs |
| `append()` | Deliver initialization and media segments |
| `buffered()` | Report buffered ranges when your platform exposes them |
| `has_buffer_tracking()` | Return `false` when your platform cannot report buffered ranges |
| `set_duration()` | Pass the presentation duration to the renderer |
| `set_timestamp_offset()` / `set_append_window()` | Map presentation timing into the renderer |
| `change_type()` | Handle codec changes between periods |
| `remove_range()` | Evict old data when your driver enables pruning |
| `end_of_stream()` | Signal that all media has been delivered |

```rust
use async_trait::async_trait;
use bytes::Bytes;
use dash_rs::{DashError, MediaBuffer, MediaType, TimeRange};

struct MyBuffer {
    // Your decoder handle, pipeline reference, or browser bridge.
}

impl MyBuffer {
    fn new() -> Self {
        Self {}
    }
}

#[async_trait(?Send)]
impl MediaBuffer for MyBuffer {
    fn add_track(&mut self, mt: MediaType, mime_type: &str) -> bool {
        // Allocate a decoder track or SourceBuffer for `mt`.
        let _ = (mt, mime_type);
        true
    }

    async fn append(
        &mut self,
        mt: MediaType,
        data: Bytes,
        is_init: bool,
    ) -> Result<(), DashError> {
        // Forward `data` to your decoder or SourceBuffer.
        let _ = (mt, data, is_init);
        Ok(())
    }

    fn buffered(&self, _mt: MediaType) -> Vec<TimeRange> {
        // Return real buffered ranges when available.
        vec![]
    }

    fn has_buffer_tracking(&self) -> bool {
        // Keep the default `true` when `buffered()` returns real ranges.
        false
    }
}
```

The sample above represents a sink that cannot report buffered ranges. Browser
MSE integrations should return `SourceBuffer.buffered` from `buffered()` and
keep the default `has_buffer_tracking() == true`.

See the complete platform integrations in
[`examples/wasm_player`](../examples/wasm_player) and
[`examples/gstreamer_player`](../examples/gstreamer_player).

## 4. Implement `PlaybackDriver`

`PlaybackDriver` reports renderer state and receives renderer actions while
`Player::run()` is active. Only `position_s()` is required.

```rust
use async_trait::async_trait;
use dash_rs::{PlaybackDriver, PlaybackState};

struct MyDriver {
    // Your renderer handle.
}

impl MyDriver {
    fn new() -> Self {
        Self {}
    }
}

#[async_trait(?Send)]
impl PlaybackDriver for MyDriver {
    fn position_s(&self) -> f64 {
        // Return the renderer's current playback position.
        0.0
    }

    fn playback_state(&self) -> PlaybackState {
        // Return Paused or Buffering when applicable.
        PlaybackState::Playing
    }
}
```

Common optional overrides:

| Method | When to override |
|---|---|
| `playback_state()` | Report user pauses and renderer stalls |
| `on_gap_seek()` | Seek the renderer when playback needs to skip a gap |
| `on_session_started()` | Seek or start the renderer after session startup |
| `wants_pts_remap()` | Return `true` for browser MSE live playback |
| `pending_seek_s()` / `has_pending_seek()` | Accept seek requests while `run()` is active |
| `throughput_override_bps()` | Supply externally measured bandwidth |
| `enable_pruning()` | Enable old-buffer eviction when your sink needs it |
| `should_continue()` | Stop playback before natural end-of-stream |

All async trait implementations must use `#[async_trait(?Send)]`.

## 5. Start playback

Construct a `Player` from a manifest URL, optionally register event handlers,
and call `run()`:

```rust
use dash_rs::{Player, PlayerEvent, Url};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = Url::parse("https://example.com/stream.mpd")?;
    let mut player = Player::from_url(
        &url,
        HttpFetcher::new(),
        MyBuffer::new(),
        MyDriver::new(),
    )
    .await?;

    player.on(|event| {
        if let PlayerEvent::Log(log) = event {
            println!("[{:?}] {}: {}", log.level, log.target, log.message);
        }
    });

    player.run().await?;
    Ok(())
}
```

`Player::from_url` fetches and parses the manifest. `Player::run()` opens the
media buffer and performs the full playback startup sequence before entering
the fetch loop.

When the manifest is already parsed, use `Player::from_mpd`. Both URL and
pre-parsed construction paths also have a `_with_clock` variant for platforms
that provide a custom clock:

| Constructor | When to use |
|---|---|
| `Player::from_url` | Standard application playback from a URL |
| `Player::from_url_with_clock` | Playback from a URL with a platform clock |
| `Player::from_mpd` | Tests or offline scenarios with an already-parsed manifest |
| `Player::from_mpd_with_clock` | Already-parsed manifest with a platform clock |

## Player events

Register callbacks with `Player::on()` before calling `run()`. Use events for
application logging, status panels, metadata handling, and playback telemetry:

```rust
use dash_rs::PlayerEvent;

player.on(|event| match event {
    PlayerEvent::Log(log) => {
        println!("[{:?}] {}: {}", log.level, log.target, log.message);
    }
    PlayerEvent::Iteration(s) => {
        println!(
            "buffer={:.1}s throughput={:.0}kbps",
            s.video_buffer_ms / 1_000.0,
            s.last_throughput_bps.unwrap_or(0.0) / 1_000.0,
        );
    }
    PlayerEvent::InbandDash(ev) => {
        println!(
            "emsg {} {:?}: {} bytes",
            ev.scheme_id_uri,
            ev.value,
            ev.message_data.len(),
        );
    }
    PlayerEvent::PlaybackStateChanged { from, to } => {
        println!("playback state {from:?} -> {to:?}");
    }
    PlayerEvent::QualityChange {
        media_type,
        old_index,
        new_index,
        ..
    } => {
        println!("{media_type:?} switched {old_index} -> {new_index}");
    }
    PlayerEvent::RunEnded { reason } => {
        println!("run ended: {reason:?}");
    }
    _ => {}
});
```

Before or after `run()`, use `Player` accessors for configuration and
inspection:

```rust
for rep in player.video_representations() {
    println!("{} kbps", rep.bandwidth / 1_000);
}

println!("live: {}", player.is_live());
println!("duration: {:?}", player.duration_s());
```

Use `player.session()` or `player.session_mut()` only when you need session
configuration that does not have a `Player` convenience method.

## Seeking

For user-initiated seeks, store the requested position in shared driver state.
`Player::run()` polls the driver and repositions media fetching for you.

```rust
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dash_rs::PlaybackDriver;

#[derive(Clone, Default)]
struct SeekHandle {
    pending_s: Arc<Mutex<Option<f64>>>,
}

impl SeekHandle {
    fn request(&self, position_s: f64) {
        *self.pending_s.lock().unwrap() = Some(position_s);
    }
}

struct SeekDriver {
    seek: SeekHandle,
}

#[async_trait(?Send)]
impl PlaybackDriver for SeekDriver {
    fn position_s(&self) -> f64 {
        0.0
    }

    fn pending_seek_s(&mut self) -> Option<f64> {
        self.seek.pending_s.lock().unwrap().take()
    }

    fn has_pending_seek(&self) -> bool {
        self.seek.pending_s.lock().unwrap().is_some()
    }
}

let seek = SeekHandle::default();
let driver = SeekDriver { seek: seek.clone() };

// From your UI handler:
seek.request(42.0);
```

Seek the renderer from your UI handler as well. `pending_seek_s()` must clear
the pending request before returning it; `Option::take()` is the usual pattern.
Override `has_pending_seek()` alongside it so seeks interrupt buffer waits
promptly.

## Custom clock

Use `Player::from_url_with_clock` on platforms where the default system clock
is unavailable, such as browser WASM integrations:

```rust
use std::sync::Arc;

let clock = Arc::new(MyPlatformClock::new());
let mut player = Player::from_url_with_clock(
    &url,
    fetcher,
    buffer,
    driver,
    clock,
)
.await?;

player.run().await?;
```

Implement the `Clock` trait with your platform's monotonic clock, UTC clock,
and async timer. The browser implementation in
[`examples/wasm_player`](../examples/wasm_player) is a complete reference.
