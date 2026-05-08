//! Platform-agnostic monotonic time helper.
//!
//! [`mono_ms`] returns milliseconds on a monotonically non-decreasing clock
//! without pulling in `std::time::Instant` on WASM (where it panics at
//! runtime).
//!
//! | Platform | Source |
//! |---|---|
//! | Native | Elapsed time since the first call, via [`std::sync::OnceLock`] + [`std::time::Instant`] |
//! | `wasm32-unknown-unknown` | `js_sys::Date::now()` (milliseconds since Unix epoch) |
//!
//! On WASM the returned value is not zero-anchored (it is the Unix epoch in
//! ms), but only *deltas* between two calls are ever used by ABR rules and
//! the CDN blacklist, so the absolute base does not matter.

/// Returns a monotonically non-decreasing timestamp in milliseconds.
///
/// Only deltas between two calls matter — the absolute base is arbitrary.
pub(crate) fn mono_ms() -> f64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1_000.0
    }
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
}
