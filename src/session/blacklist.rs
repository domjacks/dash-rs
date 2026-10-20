//! CDN URL blacklist — time-based per-origin exclusion list.
//!
//! Mirrors `BlacklistController.js` in dash.js.  When a segment fetch fails for
//! a particular CDN, its service-location key is added to this map with an expiry
//! of `now + cooldown`.  Subsequent fetch attempts skip any candidate whose key
//! is still in the map.
//!
//! If every candidate is blacklisted (the full candidate list has been exhausted),
//! the exclusion is overridden and all candidates are tried again — this prevents
//! the session from permanently stalling when no healthy CDN is available.
//!
//! The default cooldown is **600 seconds**, matching
//! `BlacklistController.BLACKLIST_DURATION` in dash.js.

use core::time::Duration;

use hashbrown::HashMap;

#[cfg(not(feature = "std"))]
use alloc::{borrow::ToOwned, string::String};

/// Default cooldown duration matching dash.js `BlacklistController.BLACKLIST_DURATION`.
pub(crate) const DEFAULT_COOLDOWN: Duration = Duration::from_secs(600);

/// Time-based CDN blacklist.
///
/// Maps *service-location keys* to expiry timestamps in milliseconds (from
/// [`crate::time::mono_ms`]).  A key is considered blacklisted while
/// `mono_ms() < expiry`.
///
/// The key is the `@serviceLocation` attribute of the contributing `<BaseURL>`
/// element, or the URL's host string when that attribute is absent.
#[derive(Debug)]
pub(super) struct CdnBlacklist {
    /// Maps service-location key → expiry timestamp in ms (from `mono_ms()`).
    entries: HashMap<String, f64>,
    cooldown: Duration,
}

impl Default for CdnBlacklist {
    fn default() -> Self {
        Self::new()
    }
}

impl CdnBlacklist {
    /// Create a blacklist with the default 600-second cooldown.
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            cooldown: DEFAULT_COOLDOWN,
        }
    }

    /// Replace the cooldown duration applied to future `record_failure` calls.
    pub(super) fn set_cooldown(&mut self, cooldown: Duration) {
        self.cooldown = cooldown;
    }

    /// Returns the current cooldown duration.
    pub(super) fn cooldown(&self) -> Duration {
        self.cooldown
    }

    /// Returns `true` if `key` is currently blacklisted (cooldown has not yet elapsed).
    pub(super) fn is_blacklisted(&self, key: &str) -> bool {
        self.entries
            .get(key)
            .map(|&expiry| crate::time::mono_ms() < expiry)
            .unwrap_or(false)
    }

    /// Record a failure for `key`, (re-)starting its cooldown window.
    ///
    /// If `key` is already in the map, its expiry is reset to `now + cooldown`.
    pub(super) fn record_failure(&mut self, key: &str) {
        let expiry = crate::time::mono_ms() + self.cooldown.as_secs_f64() * 1_000.0;
        self.entries.insert(key.to_owned(), expiry);
    }

    /// Remove entries whose cooldown has already elapsed.
    ///
    /// Called opportunistically before filtering candidates so that the map
    /// stays bounded across long-running sessions.
    pub(super) fn evict_expired(&mut self) {
        let now = crate::time::mono_ms();
        self.entries.retain(|_, expiry| now < *expiry);
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "std")]
    use std::thread;

    #[test]
    fn new_blacklist_has_no_entries() {
        let bl = CdnBlacklist::new();
        assert!(!bl.is_blacklisted("cdn1"));
        assert!(!bl.is_blacklisted("cdn2"));
    }

    #[test]
    fn record_failure_blacklists_key() {
        let mut bl = CdnBlacklist::new();
        bl.record_failure("cdn1");
        assert!(bl.is_blacklisted("cdn1"));
        assert!(
            !bl.is_blacklisted("cdn2"),
            "unrelated key must not be affected"
        );
    }

    #[test]
    #[cfg(feature = "std")]
    fn blacklist_expires_after_cooldown() {
        let mut bl = CdnBlacklist::new();
        bl.set_cooldown(Duration::from_millis(5));
        bl.record_failure("cdn1");
        assert!(
            bl.is_blacklisted("cdn1"),
            "should be blacklisted immediately after failure"
        );
        thread::sleep(Duration::from_millis(10));
        assert!(
            !bl.is_blacklisted("cdn1"),
            "should have expired after cooldown"
        );
    }

    #[test]
    #[cfg(feature = "std")]
    fn record_failure_resets_expiry() {
        let mut bl = CdnBlacklist::new();
        bl.set_cooldown(Duration::from_millis(100));
        bl.record_failure("cdn1");
        thread::sleep(Duration::from_millis(50));
        // Reset the expiry — should be blacklisted for another full cooldown.
        bl.record_failure("cdn1");
        thread::sleep(Duration::from_millis(60));
        assert!(
            bl.is_blacklisted("cdn1"),
            "expiry should have been extended by the second record_failure"
        );
    }

    #[test]
    #[cfg(feature = "std")]
    fn evict_expired_removes_stale_entries() {
        let mut bl = CdnBlacklist::new();
        bl.set_cooldown(Duration::from_millis(5));
        bl.record_failure("cdn1");
        thread::sleep(Duration::from_millis(10));
        bl.evict_expired();
        assert!(
            bl.entries.is_empty(),
            "evict_expired should remove elapsed entries"
        );
    }

    #[test]
    fn evict_expired_keeps_live_entries() {
        let mut bl = CdnBlacklist::new();
        bl.set_cooldown(Duration::from_secs(600));
        bl.record_failure("cdn1");
        bl.evict_expired();
        assert!(
            bl.is_blacklisted("cdn1"),
            "live entry must survive evict_expired"
        );
    }

    #[test]
    #[cfg(feature = "std")]
    fn set_cooldown_takes_effect_for_new_failures() {
        let mut bl = CdnBlacklist::new();
        bl.set_cooldown(Duration::from_millis(1));
        assert_eq!(bl.cooldown(), Duration::from_millis(1));
        bl.record_failure("cdn1");
        thread::sleep(Duration::from_millis(5));
        assert!(
            !bl.is_blacklisted("cdn1"),
            "short cooldown should expire quickly"
        );
    }
}
