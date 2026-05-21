# Code Review — dash-rs

Full review against: idiomatic Rust, API ergonomics, documentation, test coverage, bugs,
error handling, DASH spec compliance, and no_std compliance.

---

## Contents

- [Code Review — dash-rs](#code-review--dash-rs)
  - [Contents](#contents)
  - [1. no\_std Compliance Issues](#1-no_std-compliance-issues)
  - [2. Idiomatic Rust](#2-idiomatic-rust)
  - [3. API Ergonomics](#3-api-ergonomics)
  - [4. Documentation Gaps](#4-documentation-gaps)
    - [Still genuinely open](#still-genuinely-open)
    - [`InsufficientBufferRule` boundary: `<=` vs `<`](#insufficientbufferrule-boundary--vs-)
  - [6. Error Handling](#6-error-handling)
  - [7. Suggested Improvements and New Features](#7-suggested-improvements-and-new-features)
    - [Ergonomics](#ergonomics)
    - [Correctness improvements](#correctness-improvements)
    - [Compliance](#compliance)
    - [Documentation / examples](#documentation--examples)

---

## 1. no_std Compliance Issues

| Severity | Location | Issue |
|---|---|---|
| Low | `src/session/blacklist.rs` | `use std::thread` in `#[cfg(test)]` without a `#[cfg(feature = "std")]` guard. |
| Low | Various | `alloc::collections::VecDeque` is imported unconditionally in `abr/throughput.rs` and `abr/switch_history.rs`; other files gate it with `#[cfg(not(feature = "std"))]`. Inconsistent convention throughout the codebase. |
| Low | `src/clock.rs` | `use alloc::boxed::Box` imported unconditionally but `Box` is not directly used in the file. Dead import. |

---

## 2. Idiomatic Rust

Validated against current tree.

| Location | Issue | Status |
|---|---|---|
| `src/abr/mod.rs` | Rule-to-reason mapping dispatches on `rule.name()` (untyped string). A new rule that does not match a case silently returns `ThroughputBased`. Add `fn reason(&self) -> AbrDecisionReason` to the `AbrRule` trait and remove the match. | Open |
| `src/abr/mod.rs` | `SwitchHistoryRule` is a named field evaluated separately from `Vec<Box<dyn AbrRule>>`. `with_rules()` cannot exclude or replace it, reducing test isolation of the rule pipeline. | Open |
| `src/abr/rule.rs` | `AbrRule: Send + Sync` can block WASM-hosted rule impls that carry `!Send` state (for example `JsValue`). Consider relaxing this bound to match the crate's `?Send` integration style. | Open |
| `src/abr/buffer.rs` | `const GAMA` and public field `pub gama` are misspellings of "gamma". API rename is semver-breaking, so it needs a deprecation path. | Open |
| `src/abr/throughput.rs` | `ThroughputRule::safety_factor` is `pub`. Prefer encapsulation (`pub(crate)` + setter/new constructor) to avoid unconstrained mutation from callers. | Open |
| `src/abr/throughput.rs` | EMA seed guard uses `if self.fast_ema == 0.0`; a dedicated `seeded: bool` avoids float-sentinel coupling. | Open |
| `src/error.rs` | Error variants mix tuple-style (`AppendFailed(String)`) and struct-style (`NoRepresentations { media_type: String }`). Standardise variant shape for consistency. | Open |
| `src/error.rs` | `DashError` does not derive `PartialEq`, forcing tests to rely on `matches!` or string checks. | Open |
| `src/lib.rs` | `#[macro_use] extern crate alloc` is pre-2018 style; prefer direct macro paths/imports in Edition 2021. | Open |
| `src/media_buffer.rs` | `StubBuffer::add_track` and `StubBuffer::change_type` duplicate the same MIME-track `match`. Extract a private helper to reduce repetition. | Open |
| `src/media_buffer.rs` | `let _ = mt;` in `remove_range` is less idiomatic than naming the parameter `_mt`. | Open |
| `src/session/mod.rs` | Prior duplication between `estimated_buffer_level_ms` and `estimated_playback_position_s` pause-accounting is already extracted into `effective_elapsed_ms()`. | Fixed |

---

## 3. API Ergonomics

Ordered by consumer impact.

**1. `async_trait(?Send)` has no guard**  
Omitting `?Send` produces "associated type not found" — one of Rust's most cryptic errors.
A re-exported proc-macro alias `#[dash_rs::async_impl]` that bakes in `?Send` would eliminate
the entire class of confusion and remove the lengthy warning from every doc comment.

**2. `subscribe_events` / `DashEvent` has no example**  
It is a completed, tested feature but entirely invisible to new consumers because no example
demonstrates it. A `session.subscribe_events()` call with a printed handler in `http_fetch.rs`
or `basic.rs` would make it discoverable.

**3. WASM `lib.rs` bridge cloning is non-obvious**  
Three of the four constructors require `JsValue::clone(bridge.as_ref()).unchecked_into()`
while `WasmPlaybackDriver::new` takes `&bridge`. A `JsBridge::clone_ref()` helper method
would make all four call sites uniform.

---

## 4. Documentation Gaps

| Location | Gap |
|---|---|
| `src/clock.rs` | `set_timeout` doc does not describe behaviour for `NaN`, negative, or `INFINITY` inputs. |
| `src/clock.rs` | "Implementors may declare `unsafe impl Send/Sync`" — no safety justification for why this is sound on WASM. |
| `src/media_buffer.rs` | `open()` doc inaccurately describes MSE steps: conflates `URL.createObjectURL` (sync) with awaiting `sourceopen` (async). |
| `src/media_buffer.rs` | `has_buffer_tracking` doc does not explain why `NullBuffer` keeps the default `true` despite `buffered()` always returning `[]`. The combination looks like a bug without explanation. |
| `src/error.rs` | `EndOfStream` is `#[doc(hidden)]` but there is no comment warning `SegmentFetcher` implementors never to produce this variant themselves. |
| `src/abr/metrics.rs` | `is_post_seek` says "set automatically by `DashSession`" but the field is `pub`. Callers who construct `AbrMetrics` manually will get `false` silently. Field should be `pub(crate)`. |
| `src/abr/metrics.rs` | Doc hardcodes `0.5` as the `AbandonRequestsRule` threshold; the actual const lives in `abandon_requests.rs` and could change independently. |
| `src/session/driver.rs` | The minimal `PlaybackDriver` implementation code block appears twice verbatim in the doc comment. One copy should be removed. |
| `src/session/driver.rs` | `last_transfer_url: String` uses `""` as "no data"; `last_throughput_bps: f64` uses `0.0`. Both should use `Option` to make the absent-value case machine-checkable. |
| `AGENTS.md` | Documents `DashSession::buffered(media_type)` but this method does not appear to exist under that name. Needs verification and correction. |

---


### Still genuinely open

| Location | Missing case |
|---|---|
| `src/session/mod.rs` | `wait_for_buffer_capacity` interrupt path: pending seek fires and unblocks the gate within ≤ 500 ms. Non-trivial to test without a fake clock that can inject a seek mid-sleep. |

### `InsufficientBufferRule` boundary: `<=` vs `<`

✅ Documented by `insufficient_buffer_at_exact_threshold_fires` — the test asserts that the rule
fires (returns `Some(0)`) when `buffer_level_ms == critical_threshold_ms`, confirming the `<=`
behaviour deliberately differs from dash.js's strict `<`.

---

## 5. Error Handling

| Location | Issue |
|---|---|
| `src/error.rs` | `AllBaseUrlsFailed { last_error: String }` discards the original error type. Callers cannot distinguish HTTP 404 from a network timeout. Should use `last_error: Box<DashError>` or similar. |
| `src/manifest/mod.rs` | `String::from_utf8_lossy` silently replaces invalid UTF-8 in MPD responses. The root cause (encoding error) is masked as a generic parse error. Use `String::from_utf8` and surface the encoding failure distinctly. |
| `src/session/utc.rs` | `core::str::from_utf8(&response.data).unwrap_or("")` swallows a UTF-8 decode error with no log entry. Log at `warn!` before falling through. |
| `src/session/xlink.rs` | Same `unwrap_or("")` pattern — XLink UTF-8 errors are invisible in logs. |
| `src/fetch/mod.rs` | `fn abort(&self)` has no return type. Implementations cannot signal that an abort failed or was impossible. Should return `Result<(), FetchError>`. |
| `src/session/mod.rs` | Fetch error during a period-advance init-segment fetch propagates as a bare `DashError::FetchFailed` with no indication that it occurred during a period transition, making it hard to distinguish from a normal segment fetch failure. |

---
