//! [`BridgeClock`] — [`dash_rs::clock::Clock`] implementation backed by
//! `performance.now()` and `Date.now()` called through the [`JsBridge`].

use std::sync::Arc;

use async_trait::async_trait;
use dash_rs::clock::Clock;
use wasm_bindgen_futures::JsFuture;

use crate::bridge::JsBridge;

// ── BridgeClock ───────────────────────────────────────────────────────────────

/// [`Clock`] implementation that calls `performance.now()` and `Date.now()`
/// via the JS bridge.
///
/// Stores `start_ms` (captured at construction) so that [`Clock::now_ms`]
/// returns elapsed time rather than an absolute `performance.now()` value.
pub(crate) struct BridgeClock {
    bridge:   JsBridge,
    start_ms: f64,
}

impl BridgeClock {
    pub(crate) fn new(bridge: JsBridge) -> Arc<Self> {
        let start_ms = bridge.now_ms();
        Arc::new(Self { bridge, start_ms })
    }
}

// Safety: WASM is single-threaded.  `JsBridge` is a JS reference-counted
// handle that is never accessed from multiple threads simultaneously.
unsafe impl Send for BridgeClock {}
unsafe impl Sync for BridgeClock {}

#[async_trait(?Send)]
impl Clock for BridgeClock {
    /// Monotonic milliseconds elapsed since this clock was created.
    fn now_ms(&self) -> f64 {
        self.bridge.now_ms() - self.start_ms
    }

    /// Seconds since the Unix epoch (`Date.now() / 1000`).
    fn unix_s(&self) -> f64 {
        self.bridge.unix_now_s()
    }

    /// Resolves after `ms` milliseconds via `bridge.sleep()` (`setTimeout`).
    async fn set_timeout(&self, ms: f64) {
        JsFuture::from(self.bridge.sleep(ms)).await.ok();
    }
}
