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
| Medium | `src/session/mod.rs` | Live manifest refresh uses `SystemTime::now()` without going through `self.clock`. Any future refactor removing the `std`-feature gate on that branch would silently break no_std. |
| Low | `src/session/blacklist.rs` | `use std::thread` in `#[cfg(test)]` without a `#[cfg(feature = "std")]` guard. |
| Low | Various | `alloc::collections::VecDeque` is imported unconditionally in `abr/throughput.rs` and `abr/switch_history.rs`; other files gate it with `#[cfg(not(feature = "std"))]`. Inconsistent convention throughout the codebase. |
| Low | `src/clock.rs` | `use alloc::boxed::Box` imported unconditionally but `Box` is not directly used in the file. Dead import. |

---

## 2. Idiomatic Rust

| Location | Issue |
|---|---|
| `src/abr/mod.rs` | Rule-to-reason mapping dispatches on `rule.name()` (untyped string). A new rule that doesn't match a case silently returns `ThroughputBased`. Add `fn reason(&self) -> AbrDecisionReason` to the `AbrRule` trait and remove the match. |
| `src/abr/mod.rs` | `SwitchHistoryRule` is a named field evaluated separately from `Vec<Box<dyn AbrRule>>`. `with_rules()` cannot exclude or replace it, breaking test isolation of the rule pipeline. |
| `src/abr/rule.rs` | `AbrRule: Send + Sync` — the library uses `?Send` everywhere else for WASM compatibility. A WASM-hosted rule holding a `JsValue` (which is `!Send`) cannot implement this trait. Should use `?Send`. |
| `src/abr/buffer.rs` | `const GAMA` and public field `pub gama` — misspelling of "gamma". This is now a public API name; fixing it is a breaking change. |
| `src/abr/throughput.rs` | `ThroughputRule::safety_factor` is `pub`. Mutable public field on a type whose `AbrRule::max_index` takes `&self`. A caller can mutate it mid-select in a concurrent context. Should be `pub(crate)` or accessed via a setter. |
| `src/abr/throughput.rs` | EMA seed guard `if self.fast_ema == 0.0` — `f64` equality comparison to `0.0` is fragile. Use a separate `bool seeded` field. |
| `src/error.rs` | `NoRepresentations`, `EndOfStream`, `SegmentAbandoned`, `SegmentNotYetAvailable` use named-field struct variants (`{ media_type: String }`) while `AppendFailed(String)` and `RemoveFailed(String)` are tuple variants. Inconsistent style. |
| `src/error.rs` | `DashError` does not derive `PartialEq`. Tests are forced to use `matches!` or `.to_string()`. Most variants are `PartialEq`-derivable. |
| `src/lib.rs` | `#[macro_use] extern crate alloc` is a pre-2018 idiom. Edition 2021 imports macros via `use`. |
| `src/media_buffer.rs` | `StubBuffer::add_track` and `StubBuffer::change_type` duplicate the same `match mt { Video => …, Audio => … }` pattern. Extract a private `set_mime` helper. |
| `src/media_buffer.rs` | `let _ = mt;` in `remove_range` body. The idiomatic suppression is `_mt` in the function signature. |
| `src/session/mod.rs` | `estimated_buffer_level_ms` and `estimated_playback_position_s` contained identical pause-accounting arithmetic — extracted into `effective_elapsed_ms()` helper. |

---

## 3. API Ergonomics

Ordered by consumer impact.

**2. `async_trait(?Send)` has no guard**  
Omitting `?Send` produces "associated type not found" — one of Rust's most cryptic errors.
A re-exported proc-macro alias `#[dash_rs::async_impl]` that bakes in `?Send` would eliminate
the entire class of confusion and remove the lengthy warning from every doc comment.

**7. `subscribe_events` / `DashEvent` has no example**  
It is a completed, tested feature but entirely invisible to new consumers because no example
demonstrates it. A `session.subscribe_events()` call with a printed handler in `http_fetch.rs`
or `basic.rs` would make it discoverable.

**10. WASM `lib.rs` bridge cloning is non-obvious**  
Three of the four constructors require `JsValue::clone(bridge.as_ref()).unchecked_into()`
while `WasmPlaybackDriver::new` takes `&bridge`. A `JsBridge::clone_ref()` helper method
would make all four call sites uniform.

---

## 4. Documentation Gaps

| Location | Gap |
|---|---|
| `src/lib.rs` | Crate-level doc comment is a stub. No quick-start example, no feature-flag table, no overview of the three core traits. |
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

## 6. Error Handling

| Location | Issue |
|---|---|
| `src/error.rs` | `AllBaseUrlsFailed { last_error: String }` discards the original error type. Callers cannot distinguish HTTP 404 from a network timeout. Should use `last_error: Box<DashError>` or similar. |
| `src/manifest/mod.rs` | `String::from_utf8_lossy` silently replaces invalid UTF-8 in MPD responses. The root cause (encoding error) is masked as a generic parse error. Use `String::from_utf8` and surface the encoding failure distinctly. |
| `src/session/utc.rs` | `core::str::from_utf8(&response.data).unwrap_or("")` swallows a UTF-8 decode error with no log entry. Log at `warn!` before falling through. |
| `src/session/xlink.rs` | Same `unwrap_or("")` pattern — XLink UTF-8 errors are invisible in logs. |
| `src/fetch/mod.rs` | `fn abort(&self)` has no return type. Implementations cannot signal that an abort failed or was impossible. Should return `Result<(), FetchError>`. |
| `src/session/mod.rs` | Fetch error during a period-advance init-segment fetch propagates as a bare `DashError::FetchFailed` with no indication that it occurred during a period transition, making it hard to distinguish from a normal segment fetch failure. |

---

## 7. Suggested Improvements and New Features

Ordered by consumer impact.

### Ergonomics

1. **Re-export `MediaType` at `src/lib.rs` crate root.**  
   Zero-cost. Removes a forced second import from every `MediaBuffer` / `PlaybackDriver` implementor.

2. **`ThroughputCell` public newtype.**  
   ```rust
   pub struct ThroughputCell(AtomicU64);
   impl ThroughputCell {
       pub fn set(&self, bps: f64) { … }
       pub fn get(&self) -> Option<f64> { … }
   }
   ```
   Eliminates `Arc<AtomicU64>` + `f64::to_bits`/`from_bits` boilerplate from all three example
   platforms.

3. **`fn reason(&self) -> AbrDecisionReason` on `AbrRule` trait.**  
   Removes the string-dispatch match in `AbrController::select` and makes custom rules
   first-class — they can declare their own reason without being added to an internal match arm.

4. **`Player::video_representations()` / `audio_representations()` forwarders.**  
   Consistent with the existing `is_live()`, `duration_s()`, and `seek_to()` forwarders.

5. **`Display` for `QualityDecision`.**  
   Removes repeated `unwrap_or_else(|| ("done".into(), "-".into()))` boilerplate from every
   `on_iteration` implementation.

6. **`DashError: PartialEq`.**  
   Enables `assert_eq!` throughout tests instead of `matches!` / `.to_string()`.

7. **`#[non_exhaustive]` on `RequestType`, `FetchError`, `AbrDecisionReason`.**  
   No behaviour change. Prevents future variant additions from being semver-breaking changes.

8. **`#[dash_rs::async_impl]` proc-macro alias for `#[async_trait(?Send)]`.**  
   Bakes in the mandatory `?Send` bound so callers cannot accidentally omit it. Eliminates the
   lengthy warning repeated in every doc comment.

### Correctness improvements

9. **`parse_rfc2822_date` timezone validation.** ✅ Fixed (B8).

10. **`EssentialProperty` → `DashError::UnsupportedEssentialProperty`.**  
    Spec-required. Promoted to P1-11 in COMPLIANCE.md.

11. **`AllBaseUrlsFailed` preserve original error type.**
    Change `last_error: String` to `last_error: Box<DashError>` so callers can distinguish
    HTTP 404 from a network timeout.

12. **`SwitchHistoryRule` no_std fix.** ✅ Fixed.  
    `record_switch` now accepts `now_ms: f64`; `max_index` reads `metrics.now_ms`.  Both are
    sourced from `Clock::now_ms()` in `DashSession` — the rule is now correct on `no_std`.

### Compliance

13. **Parse `<EssentialProperty>` and raise an error for unrecognised values.** See P1-11 in COMPLIANCE.md.  
14. **Split DRM `<ContentProtection>` into: (a) parse + expose `pssh` bytes (P1-12), (b) CDM/EME integration (P2-13).** See COMPLIANCE.md.  
15. **Implement `@bitstreamSwitching`** — one field + one branch. See P2-2.  
16. **Implement `@qualityRanking`** — one field + secondary sort key. See P2-1.

### Documentation / examples

17. **Add a crate-level doc comment with quick-start and feature-flag table.**  
18. **Add a `subscribe_events` / `DashEvent` demo to `basic.rs` or `http_fetch.rs`.**  
19. **Fix `http_fetch.rs` `position_s()` to advance with segment time** (currently hardcodes
    `0.0`, teaching incorrect usage).

---
