# Code Review — dash-rs

Review focus: API ergonomics, documentation, error handling, no_std behaviour,
Player-first integration, and practical next tasks.

---

## 1. Current Status

Items that were previously noted but are now resolved:

| Area | Status |
|---|---|
| no_std build | `cargo check --no-default-features` passes. |
| `events` feature | Removed; `PlayerEvent`, `DashEvent`, and `Player::on` are always available. |
| `async-broadcast` dependency | Removed. Events are Player-owned and dependency-free. |
| `tracing` as a published dependency | Removed from normal dependencies. Examples may still use `tracing`. |
| no-op `tracing` shim | Removed with the old internal `tracing::...` call sites. |
| WASM status UI | Now powered by `Player::on` and `PlayerEvent::Iteration`. |

---

## 2. API Ergonomics

Ordered by consumer impact.

**1. `async_trait(?Send)` has no guard**

Omitting `?Send` still produces one of Rust's more confusing errors. The docs
warn about this heavily, but a re-exported proc-macro alias such as
`#[dash_rs::async_impl]` that bakes in `?Send` would eliminate the whole class
of mistakes.

**2. `PlayerEvent::EndOfStream` semantics are too broad**

`Player::run()` currently emits `PlayerEvent::EndOfStream` whenever the inner
`DashSession::run()` returns `Ok(())`. That includes natural EOS, but also early
exit via `PlaybackDriver::should_continue() == false`. Consumers cannot tell
whether the media ended or the driver stopped scheduling.

Prefer replacing or complementing it with:

```rust
pub enum PlayerRunEndReason {
    EndOfStream,
    StoppedByDriver,
}

pub enum PlayerEvent {
    RunEnded { reason: PlayerRunEndReason },
    // ...
}
```

**3. `Player::on` examples are still limited**

The WASM player uses `Player::on` for its status UI, but the simpler native
examples do not demonstrate event handling. Add a small `event_monitor` example
or wire `Player::on` into `http_fetch.rs` so non-browser consumers see the
recommended integration path.

**4. WASM `JsBridge` cloning is non-obvious**

Most WASM components clone the bridge with
`JsValue::clone(bridge.as_ref()).unchecked_into()`, while
`WasmPlaybackDriver::new` takes `&bridge`. A small helper such as
`clone_bridge(&JsBridge) -> JsBridge` or `JsBridgeExt::clone_ref()` would make
the construction path easier to scan.

**5. `Player::on` handler re-entrancy is undocumented**

Handlers are stored behind `Rc<RefCell<Vec<_>>>`. If a handler causes nested
event emission, it may panic at runtime due to a `RefCell` borrow conflict. This
is probably acceptable, but should be documented as "handlers must not call back
into `Player::run`/event-emitting APIs" or made robust with a queued dispatch
model.

---

## 3. Logging and Diagnostics

**1. `PlayerEvent::Log` should be explicitly scoped**

`PlayerEvent::Log` is useful, but should be documented as high-level,
consumer-facing diagnostics rather than a complete replacement for internal
debug tracing. This avoids surprising users who expect every old low-level
`tracing::debug!` to appear as a Player event.

---

## 4. Documentation Gaps

| Location | Gap |
|---|---|
| `src/clock.rs` | `Clock::set_timeout` does not document behaviour for `NaN`, negative, or `INFINITY`, even though `SystemClock` clamps these to `0`. |
| `src/clock.rs` / README WASM clock example | "Implementors may declare `unsafe impl Send/Sync`" needs a clearer safety rationale for single-threaded WASM bridge types. |
| `src/abr/metrics.rs` | `is_post_seek` says callers do not need to set it manually, but the field is public. Either make it `pub(crate)` or document manual construction requirements more explicitly. |
| `src/abr/metrics.rs` | `effective_download_ratio` doc hardcodes `0.5` as the abandon threshold. The rule constant lives elsewhere and could drift. |
| `src/session/driver.rs` | Minimal `PlaybackDriver` implementation appears twice in the trait docs. One copy can be removed. |
| `src/session/driver.rs` / `src/player.rs` | `last_transfer_url: String` uses `""` as "no data"; `last_transfer_ms: f64` uses `0.0`. `Option` would make absent values machine-checkable. |

---

## 5. Error Handling

| Location | Issue |
|---|---|
| `src/error.rs` | `AllBaseUrlsFailed { last_error: String }` discards the original error type. Callers cannot distinguish HTTP 404 from timeout/network failure. Prefer preserving structured cause data. |
| `src/manifest/mod.rs` | `String::from_utf8_lossy` silently replaces invalid UTF-8 in MPD responses. Use `String::from_utf8` and surface encoding failure distinctly. |
| `src/session/utc.rs` | `core::str::from_utf8(&response.data).unwrap_or("")` swallows UTF-8 errors. Return/emit a visible failure instead of falling through silently. |
| `src/session/xlink.rs` | Same `unwrap_or("")` pattern for XLink fetches; invalid UTF-8 is invisible. |
| `src/fetch/mod.rs` | `SegmentFetcher::abort(&self)` cannot report that abort failed or is unsupported. Consider `Result<(), FetchError>` if callers need observability. |
| `src/session/mod.rs` | Fetch errors during period-boundary init segment fetches propagate as normal fetch failures with no phase context. Add context so callers can distinguish period transition failures. |

---

## 6. Suggested Next Tasks

High leverage, ordered.

1. **Fix `PlayerEvent::EndOfStream` semantics**

   Add an explicit run-end reason so UI and automation can distinguish natural
   EOS from driver-requested stop.

2. **Add a native Player-event example**

   Create `examples/event_monitor.rs` or update `examples/http_fetch.rs` to use
   `Player::on` for `Log`, `Iteration`, `QualityChange`, and `InbandDash`.

3. **Improve UTF-8 failure handling**

   Replace lossy/silent manifest, UTC, and XLink UTF-8 handling with explicit
   errors or visible Player log events.

4. **Preserve fetch error structure through CDN fallback**

   Replace `AllBaseUrlsFailed { last_error: String }` with a structured cause
   representation.

5. **Tighten docs around manual integration**

   Update `Clock`, `AbrMetrics`, and `PlaybackDriver` docs. These are small
   documentation-only tasks with high consumer value.

6. **Consider an `async_trait(?Send)` helper**

   If a proc-macro alias is too much, add a prominent snippet and maybe a
   compile-fail doc test showing the correct annotation.
