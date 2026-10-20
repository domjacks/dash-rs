//! [`PlayerSettings`] — construction-time tuning for a [`DashSession`] / [`Player`].
//!
//! Every default mirrors the equivalent dash.js setting so that an unmodified
//! `PlayerSettings::default()` behaves exactly like a session built with the
//! plain constructors.
//!
//! ```
//! use dash_rs::PlayerSettings;
//!
//! let mut settings = PlayerSettings::default();
//! settings.buffer.buffer_time_default_ms = 20_000.0;
//! settings.abr.insufficient_buffer_threshold_ms = 4_000.0;
//! settings.live.safety_segments = 2;
//! ```
//!
//! [`DashSession`]: crate::session::DashSession
//! [`Player`]: crate::player::Player

use core::time::Duration;

use crate::session::GapControllerSettings;

/// Default session buffer target in ms (dash.js `bufferTimeDefault`).
pub const DEFAULT_BUFFER_TIME_MS: f64 = 12_000.0;
/// Default behind-playhead retention window in seconds (dash.js `bufferToKeep`).
pub const DEFAULT_BUFFER_TO_KEEP_S: f64 = 30.0;

/// Top-level settings passed to the `*_with_settings` constructors.
#[derive(Debug, Clone, Default)]
pub struct PlayerSettings {
    /// Buffer scheduling and pruning.
    pub buffer: BufferSettings,
    /// ABR rule thresholds.
    pub abr: AbrSettings,
    /// Live-edge positioning.
    pub live: LiveSettings,
    /// Gap / stall controller tuning.
    pub gaps: GapControllerSettings,
    /// CDN failover behaviour.
    pub cdn: CdnSettings,
}

/// Buffer scheduling and pruning settings.
#[derive(Debug, Clone)]
pub struct BufferSettings {
    /// Buffer fill target in ms; fetching pauses once the buffer reaches it.
    ///
    /// For VOD the effective target is `max(buffer_time_default_ms,
    /// MPD@minBufferTime)`; for live this value is used as-is.
    ///
    /// dash.js `streaming.buffer.bufferTimeDefault` — default **12 000 ms**.
    pub buffer_time_default_ms: f64,

    /// Seconds of media to keep behind the playhead when pruning.
    ///
    /// dash.js `streaming.buffer.bufferToKeep` — default **30 s**.
    pub buffer_to_keep_s: f64,
}

impl Default for BufferSettings {
    fn default() -> Self {
        Self {
            buffer_time_default_ms: DEFAULT_BUFFER_TIME_MS,
            buffer_to_keep_s: DEFAULT_BUFFER_TO_KEEP_S,
        }
    }
}

/// Thresholds for the standard ABR rule pipeline built by
/// [`AbrController::from_settings`][crate::abr::AbrController::from_settings].
#[derive(Debug, Clone)]
pub struct AbrSettings {
    /// Fraction of measured throughput treated as usable by `ThroughputRule`.
    /// Default **0.9**.
    pub throughput_safety_factor: f64,

    /// Buffer level (ms) below which `BufferOccupancyRule` forces a down-switch.
    /// Default **12 000 ms**.
    pub bola_target_buffer_ms: f64,

    /// Buffer level (ms) above which `BufferOccupancyRule` allows the highest
    /// quality. Default **30 000 ms**.
    pub bola_max_buffer_ms: f64,

    /// `BufferOccupancyRule` Lyapunov parameter V. Default **5.0**.
    pub bola_gamma: f64,

    /// Buffer level (ms) at or below which `InsufficientBufferRule` forces the
    /// lowest quality. Default **2 000 ms**.
    pub insufficient_buffer_threshold_ms: f64,

    /// Minimum time between up-switches enforced by `SwitchHistoryRule`.
    /// `None` disables the rule. Default **8 s**.
    pub min_switch_interval: Option<Duration>,

    /// Dropped-frame ratio above which `DroppedFramesRule` forces the lowest
    /// quality. Default **0.10**.
    pub dropped_frames_threshold: f64,

    /// Effective download ratio below which `AbandonRequestsRule` forces the
    /// lowest quality. Default **0.5**.
    pub abandon_multiplier: f64,
}

impl Default for AbrSettings {
    fn default() -> Self {
        use crate::abr::{
            abandon_requests, buffer, dropped_frames, insufficient_buffer, switch_history,
            throughput,
        };
        Self {
            throughput_safety_factor: throughput::SAFETY_FACTOR,
            bola_target_buffer_ms: buffer::TARGET_BUFFER_MS,
            bola_max_buffer_ms: buffer::MAX_BUFFER_MS,
            bola_gamma: buffer::GAMMA,
            insufficient_buffer_threshold_ms: insufficient_buffer::CRITICAL_BUFFER_THRESHOLD_MS,
            min_switch_interval: Some(switch_history::MIN_SWITCH_INTERVAL),
            dropped_frames_threshold: dropped_frames::DROP_THRESHOLD,
            abandon_multiplier: abandon_requests::ABANDON_MULTIPLIER,
        }
    }
}

/// Live-stream settings.
#[derive(Debug, Clone)]
pub struct LiveSettings {
    /// Segments behind the live edge to start at when the MPD has no
    /// `@suggestedPresentationDelay`. Default **3**.
    pub safety_segments: u64,
}

impl Default for LiveSettings {
    fn default() -> Self {
        Self {
            safety_segments: crate::session::live::DEFAULT_LIVE_SAFETY_SEGMENTS,
        }
    }
}

/// CDN failover settings.
#[derive(Debug, Clone)]
pub struct CdnSettings {
    /// How long a failing CDN is skipped. `Duration::ZERO` disables
    /// blacklisting.
    ///
    /// dash.js `BlacklistController.BLACKLIST_DURATION` — default **600 s**.
    pub blacklist_cooldown: Duration,
}

impl Default for CdnSettings {
    fn default() -> Self {
        Self {
            blacklist_cooldown: crate::session::blacklist::DEFAULT_COOLDOWN,
        }
    }
}
