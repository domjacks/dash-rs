//! DASH session coordinator.
//!
//! [`DashSession`] is the top-level object that drives a DASH playback
//! session.  It mirrors the responsibilities shared between `MediaPlayer.js`,
//! `Stream.js`, and `StreamProcessor.js` in dash.js:
//!
//! - Holds the parsed [`MPD`] manifest.
//! - Maintains per-track state (current quality, segment number, sink).
//! - Runs the [`AbrController`] to select quality for each fetch.
//! - Dispatches `FetchRequest`s via a [`SegmentFetcher`].
//! - Feeds fetched data into the appropriate [`SourceBufferSink`].
//!
//! # Sub-modules
//!
//! | Module | Contents |
//! |---|---|
//! | `live` | [`compute_live_edge_segment`] — live-edge snapping formula |
//! | `mpd` | `merge_segment_templates`, `extract_segment_timing`, `find_adaptation`, etc. |
//! | `timeline` | [`find_timeline_segment`], `count_timeline_segments` — `$Time$` / SegmentTimeline walking |
//! | `segment_url` | [`expand_template`], `collect_candidate_base_urls`, `resolve_effective_base_url` |
//! | `utc` | `resolve_utc_timing`, `query_http_utc` — `UTCTiming` clock-sync |

mod blacklist;
pub mod driver;
mod gap;
mod live;
pub mod metrics;
mod mpd;
mod segment_url;
mod timeline;
mod utc;
mod xlink;

pub use gap::{GapController, GapControllerSettings, GapSeekReason, GapSeekTarget};
pub use metrics::{BufferLevelSample, DashMetrics, HttpRequestRecord, RepresentationSwitchRecord};

use alloc::sync::Arc;
use hashbrown::HashMap;

#[cfg(not(feature = "std"))]
use alloc::{boxed::Box, string::{String, ToString}, vec::Vec};
#[cfg(not(feature = "std"))]
use crate::math::FloatExt as _;

use async_broadcast;
use bytes::Bytes;
use crate::manifest::mpd_types::{AdaptationSet, MPD, Period, Representation};
use futures::future::{self as fut, Either};
use futures_timer::Delay;
use url::Url;

use crate::{
    abr::{AbrController, AbrMetrics, QualityDecision, RepresentationInfo},
    clock::{Clock, SystemClock},
    error::DashError,
    fetch::{FetchRequest, FetchResponse, RequestType, SegmentFetcher},
    manifest::ManifestLoader,
    media_source::{
        sink::{SourceBufferSink, StreamInfo},
        Chunk, MediaSource,
    },
    mp4::{parse_all_emsg, parse_prft, parse_sidx, parse_sidx_entries, ProducerReferenceTime, SidxEntry},
};
use crate::abr::throughput::ThroughputTracker;

use self::{
    live::compute_live_edge_segment,
    mpd::{
        adaptation_mime_type, extract_representations, extract_segment_list_timing,
        extract_segment_timing, find_adaptation, find_segment_base, merge_segment_lists,
        merge_segment_templates, segment_base_media_path,
    },
    segment_url::{collect_candidate_base_urls, expand_template, resolve_effective_base_url, CandidateUrl},
    timeline::{find_timeline_segment, snap_to_timeline_grid},
};
use blacklist::CdnBlacklist;

// ── MediaType ─────────────────────────────────────────────────────────────────

/// Which media track a session operation targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaType {
    Video,
    Audio,
}

impl core::fmt::Display for MediaType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MediaType::Video => write!(f, "video"),
            MediaType::Audio => write!(f, "audio"),
        }
    }
}

// ── DashEvent ─────────────────────────────────────────────────────────────────

/// An in-band DASH event extracted from a `DASHEventMessageBox` (`emsg`) in a
/// media segment.
///
/// Subscribers receive these via the channel returned by
/// [`DashSession::subscribe_events`].  The channel is an
/// `async-broadcast` channel with a buffer of 64 events; slow
/// consumers may see [`async_broadcast::RecvError::Overflowed`] if
/// they fall too far behind.
///
/// # Presentation time semantics
///
/// | `emsg` version | `presentation_time_s` meaning |
/// |---|---|
/// | 0 | Seconds relative to the segment's earliest presentation time.  Add `sidx.start_s` (if available) to get an absolute timeline position. |
/// | 1 | Absolute seconds from the start of the media presentation timeline. |
///
/// # Example
///
/// ```no_run
/// # use dash_rs::session::{DashSession, MediaType};
/// # use dash_rs::fetch::stub::StubSegmentFetcher;
/// # use dash_rs::media_source::stub::StubMediaSource;
/// # use std::sync::Arc;
/// # use url::Url;
/// # async fn example() -> Result<(), dash_rs::error::DashError> {
/// # let mut media_source = StubMediaSource::new();
/// # media_source.open();
/// # let url = Url::parse("https://example.com/manifest.mpd").unwrap();
/// let mut session = DashSession::from_url(&url, StubSegmentFetcher, media_source).await?;
///
/// // Subscribe *before* the fetch loop so no events are missed.
/// let mut events = session.subscribe_events();
///
/// tokio::spawn(async move {
///     while let Ok(ev) = events.recv().await {
///         println!("event: {} @ {:.3}s", ev.scheme_id_uri, ev.presentation_time_s);
///     }
/// });
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct DashEvent {
    /// Which media track's segment contained this event.
    pub media_type: MediaType,
    /// URI identifying the event scheme (e.g. `"urn:scte:scte35:2014:xml+bin"`).
    pub scheme_id_uri: String,
    /// Optional value qualifying the scheme (empty string when the `emsg`
    /// field is absent or empty).
    pub value: String,
    /// Presentation time in seconds (see table above for version semantics).
    pub presentation_time_s: f64,
    /// Duration of the event in seconds.  [`f64::INFINITY`] when the `emsg`
    /// signals an indefinite duration (`event_duration == 0xFFFF_FFFF`).
    pub duration_s: f64,
    /// Unique identifier for this event within its scheme.
    pub id: u32,
    /// Raw event message payload bytes.
    pub message_data: Bytes,
}

/// Capacity of the in-band event broadcast channel.
///
/// A slow consumer that falls more than this many events behind will receive
/// [`tokio::sync::broadcast::error::RecvError::Lagged`].
const EVENT_CHANNEL_CAPACITY: usize = 64;

// ── AddressingMode ────────────────────────────────────────────────────────────

/// How segment URLs are constructed for a given track.
///
/// Determined at session construction from the MPD hierarchy and drives the
/// URL-building branch inside [`DashSession::fetch_next_segment`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum AddressingMode {
    /// `SegmentTemplate` with `$Number$` substitution (fixed-duration segments).
    Number,
    /// `SegmentTemplate` with `SegmentTimeline` / `$Time$` substitution.
    Timeline,
    /// Explicit `<SegmentList>` with `<SegmentURL>` elements.  The segment
    /// index into `segment_urls` is `segment_number` (which starts at 0 for
    /// SegmentList tracks).
    List,
    /// `SegmentBase` byte-range addressing.
    ///
    /// `media_url` is the relative path of the media file (typically a
    /// `BaseURL` element value; empty when the effective base URL is the file
    /// itself).  `index_range` is `(start, end_inclusive)` of the `sidx` box
    /// inside the media file.  `init_range` is the optional
    /// `SegmentBase.Initialization@range` — when `None`, a `SegmentBase`
    /// `Initialization.sourceURL` (if present) or the entire file is used.
    Base {
        media_url: String,
        index_range: (u64, u64),
        init_range: Option<(u64, u64)>,
    },
}

// ── TrackReinit ───────────────────────────────────────────────────────────────

/// Computed data needed to reinitialise a [`TrackState`] when crossing an
/// MPD `Period` boundary.
///
/// All fields are derived from the new period's `AdaptationSet` before any
/// mutable borrows are taken, so the immutable borrow on the [`MPD`] can be
/// released before tracks are mutated.
struct TrackReinit {
    representations: Vec<RepresentationInfo>,
    mime: String,
    start_number: u64,
    seg_dur_ms: f64,
    total_segments: Option<u64>,
    addressing_mode: AddressingMode,
    /// MSE `SourceBuffer.timestampOffset` = `Period@start − PTO/timescale`.
    timestamp_offset_s: f64,
    /// `Period@start` in seconds (0.0 for the first/only period).
    ///
    /// Used by [`seek_to`] to map presentation-timeline positions back to
    /// segment numbers: `N = start_number + ⌊(pos − period_start_s) / seg_dur_s⌋`.
    period_start_s: f64,
    /// Timescale ticks per second from merged `SegmentTemplate@timescale` (default 1).
    timescale: u64,
    /// `SegmentTemplate@availabilityTimeOffset` in seconds (0.0 when absent).
    ///
    /// Passed to [`compute_live_edge_segment`] so that low-latency CMAF streams
    /// can access partially-produced segments at the live edge.
    availability_time_offset_s: f64,
}

/// Compute a [`TrackReinit`] from the given adaptation set and period.
///
/// This is a pure function with no side effects.  It replicates the
/// addressing-mode logic from [`DashSession::from_mpd_with_utc`] so that
/// period transitions can reuse the same code path.
fn compute_track_reinit(
    adaptation: &AdaptationSet,
    period: &Period,
    presentation_duration: Option<core::time::Duration>,
) -> TrackReinit {
    let representations = extract_representations(adaptation);
    let mime = adaptation_mime_type(adaptation);
    let first_repr = adaptation.representations.first();

    let (start_number_tmpl, seg_dur_ms_tmpl, total_tmpl) =
        extract_segment_timing(adaptation, period, presentation_duration);

    let tmpl = merge_segment_templates(first_repr, adaptation, period);
    let has_segment_list = merge_segment_lists(first_repr, adaptation, period).is_some();

    // Extract timescale for SegmentTimeline seek re-anchoring (DASH spec §5.3.9.4).
    let timescale: u64 = tmpl.as_ref().and_then(|t| t.timescale).unwrap_or(1);

    // Extract @availabilityTimeOffset for low-latency CMAF live-edge widening.
    let availability_time_offset_s: f64 =
        tmpl.as_ref().and_then(|t| t.availabilityTimeOffset).unwrap_or(0.0);

    // Warn when @availabilityTimeComplete=false is present — chunked segment
    // delivery is not yet supported (P1-4).
    if tmpl.as_ref().and_then(|t| t.availabilityTimeComplete) == Some(false) {
        tracing::warn!(
            "SegmentTemplate@availabilityTimeComplete=false detected; \
             chunked (partial) segment delivery is not yet supported — \
             fetching will wait for complete segments (P1-4)"
        );
    }

    // DASH spec §5.3.9.6: timestampOffset = Period@start − PTO/timescale.
    // Period@start is the presentation-timeline start of this period (0 for the
    // first/only period; non-zero in multi-period VOD).  PTO shifts decode
    // timestamps so that DTS=PTO maps to presentation time Period@start.
    let timestamp_offset_s = {
        let pto_s = tmpl.as_ref()
            .and_then(|t| {
                let pto = t.presentationTimeOffset?;
                let ts = t.timescale.unwrap_or(1);
                Some(pto as f64 / ts as f64)
            })
            .unwrap_or(0.0);
        let period_start_s = period.start.map(|d| d.as_secs_f64()).unwrap_or(0.0);
        period_start_s - pto_s
    };

    let (start_number, seg_dur_ms, total_segments, addressing_mode) =
        if let Some(ref t) = tmpl {
            if t.SegmentTimeline.is_some() {
                (start_number_tmpl, seg_dur_ms_tmpl, total_tmpl, AddressingMode::Timeline)
            } else {
                (start_number_tmpl, seg_dur_ms_tmpl, total_tmpl, AddressingMode::Number)
            }
        } else if has_segment_list {
            let (sn, sd, ts) = extract_segment_list_timing(adaptation, period);
            (sn, sd, ts, AddressingMode::List)
        } else if let Some(sb) = find_segment_base(first_repr, adaptation, period) {
            let media_url = segment_base_media_path(first_repr, adaptation);
            let index_range = sb.indexRange.as_deref()
                .and_then(parse_byte_range)
                .unwrap_or((0, 0));
            let init_range = sb.Initialization.as_ref()
                .and_then(|i| i.range.as_deref())
                .and_then(parse_byte_range);
            (0, seg_dur_ms_tmpl, None, AddressingMode::Base {
                media_url,
                index_range,
                init_range,
            })
        } else {
            (start_number_tmpl, seg_dur_ms_tmpl, total_tmpl, AddressingMode::Number)
        };

    TrackReinit {
        representations,
        mime,
        start_number,
        seg_dur_ms,
        total_segments,
        addressing_mode,
        timestamp_offset_s,
        period_start_s: period.start.map(|d| d.as_secs_f64()).unwrap_or(0.0),
        timescale,
        availability_time_offset_s,
    }
}

/// Apply a [`TrackReinit`] to an existing [`TrackState`] in-place.
///
/// Updates the MIME type (via `change_type` if it changed), append window,
/// timestamp offset, and all timing/addressing fields.  Intentionally
/// preserves `throughput_tracker`, `last_transfer_ms`, and `current_index`
/// for ABR continuity across period boundaries.
async fn apply_track_reinit(
    track: &mut TrackState,
    ri: &TrackReinit,
    stream_info: &StreamInfo,
    media_source_duration: Option<f64>,
) -> Result<(), DashError> {
    if track.sink.mime_type() != ri.mime {
        track.sink.change_type(&ri.mime).await?;
    }
    track.sink.update_append_window(stream_info, media_source_duration);
    track.sink.set_timestamp_offset(ri.timestamp_offset_s);
    track.representations = ri.representations.clone();
    track.segment_number = ri.start_number;
    track.start_number = ri.start_number;
    track.initial_segment_number = ri.start_number;
    track.segment_duration_ms = ri.seg_dur_ms;
    track.total_segments = ri.total_segments;
    track.addressing_mode = ri.addressing_mode.clone();
    track.next_segment_time = None;
    track.segment_byte_ranges = None;
    track.init_segment_sent = false;
    track.period_start_s = ri.period_start_s;
    track.timescale = ri.timescale;
    track.availability_time_offset_s = ri.availability_time_offset_s;
    Ok(())
}

// ── TrackState ────────────────────────────────────────────────────────────────

/// Per-track playback and buffer state maintained by [`DashSession`].
struct TrackState {
    _media_type: MediaType,
    /// Representations sorted ascending by bandwidth.
    representations: Vec<RepresentationInfo>,
    /// Index of the representation currently being downloaded.
    current_index: usize,
    /// The segment number to request next.  Initialised to `start_number`
    /// (from `SegmentTemplate@startNumber`, default 1).
    segment_number: u64,
    /// First segment number declared in the manifest (SegmentTemplate@startNumber,
    /// default 1).  Used to compute how many segments have been fetched for
    /// the EOS segment-count check.
    start_number: u64,
    /// The segment number at which *this session* began fetching.  For VOD
    /// streams this equals `start_number`.  For live streams it is set to the
    /// live-edge segment after snapping so that
    /// [`DashSession::estimated_buffer_level_ms`] counts only the segments
    /// pushed in this session rather than all segments since the beginning of
    /// the broadcast.
    initial_segment_number: u64,
    /// Duration of a single segment in milliseconds, derived from
    /// `SegmentTemplate@duration / SegmentTemplate@timescale * 1000`.
    /// Used for buffer-level estimation.
    segment_duration_ms: f64,
    /// Total number of segments in this track.
    /// `None` for live streams or manifests without a declared duration.
    total_segments: Option<u64>,
    /// The `SourceBufferSink` wrapping the underlying buffer.
    sink: SourceBufferSink,
    /// How segment URLs are constructed for this track.
    addressing_mode: AddressingMode,
    /// For `$Time$` addressing: the start time (in timescale ticks) of the
    /// **next** segment to fetch.  `None` means start from the first entry in
    /// the current timeline.  Ignored when `uses_timeline` is `false`.
    next_segment_time: Option<u64>,
    /// Dual-EWMA + harmonic-mean throughput estimator, updated after every
    /// successful media-segment fetch.
    ///
    /// Seeded on the first sample; returns `0.0` until then via
    /// [`ThroughputTracker::estimate`].  Replaces the old single-α EMA
    /// to better match dash.js's `ThroughputController.js` model.
    throughput_tracker: ThroughputTracker,
    /// Transfer duration of the most recently completed media-segment fetch,
    /// in milliseconds.
    ///
    /// Used as a proxy for network latency in [`AbrMetrics::latency_ms`].
    /// Note: this is the total transfer time (bytes on the wire), not a pure
    /// round-trip time measurement.  It will be `0.0` until the first segment
    /// has been fetched.
    last_transfer_ms: f64,
    /// Subsegment byte-range table for `AddressingMode::Base` tracks.
    ///
    /// Populated lazily on the first [`DashSession::fetch_next_segment`] call
    /// by fetching and parsing the `sidx` index.  `None` until populated.
    segment_byte_ranges: Option<Vec<SidxEntry>>,
    /// `true` after the init segment has been successfully appended for the
    /// current period.  Reset to `false` by [`apply_track_reinit`] at period
    /// boundaries so that the new period's init segment is fetched automatically.
    init_segment_sent: bool,
    /// `Period@start` in seconds (0.0 for the first/only period).
    ///
    /// Used by [`DashSession::seek_to`] to map a presentation-timeline position
    /// back to a segment number:
    /// `N = start_number + ⌊(position_s − period_start_s) / seg_dur_s⌋`.
    period_start_s: f64,
    /// Timescale ticks per second from the merged `SegmentTemplate@timescale`.
    ///
    /// Used by [`DashSession::seek_to`] to convert a wall-clock seek position
    /// into `next_segment_time` ticks for `SegmentTimeline` (`$Time$`) tracks,
    /// so that the timeline walk starts from the correct entry rather than the
    /// beginning of the timeline (which would be wrong for live streams with
    /// large absolute tick values).  Default 1.
    timescale: u64,
    /// `SegmentTemplate@availabilityTimeOffset` in seconds (0.0 when absent).
    ///
    /// Passed to [`compute_live_edge_segment`] so that low-latency CMAF streams
    /// can access partially-produced segments at the live edge.  A positive ATO
    /// widens the effective fetch window by `ATO` seconds.
    availability_time_offset_s: f64,
    /// The most recently decoded `ProducerReferenceTimeBox` (`prft`) from a
    /// fetched media segment, or `None` if no `prft` has been seen yet.
    ///
    /// Callers can read this via [`DashSession::last_producer_reference_time`]
    /// to compute end-to-end latency for low-latency live streams.
    last_prft: Option<ProducerReferenceTime>,
}

impl TrackState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        media_type: MediaType,
        representations: Vec<RepresentationInfo>,
        sink: SourceBufferSink,
        start_number: u64,
        segment_duration_ms: f64,
        total_segments: Option<u64>,
        addressing_mode: AddressingMode,
        period_start_s: f64,
        timescale: u64,
        availability_time_offset_s: f64,
    ) -> Self {
        Self {
            _media_type: media_type,
            representations,
            current_index: 0,
            segment_number: start_number,
            start_number,
            initial_segment_number: start_number,
            segment_duration_ms,
            total_segments,
            sink,
            addressing_mode,
            next_segment_time: None,
            throughput_tracker: ThroughputTracker::new(),
            last_transfer_ms: 0.0,
            segment_byte_ranges: None,
            init_segment_sent: false,
            period_start_s,
            timescale,
            availability_time_offset_s,
            last_prft: None,
        }
    }
}

// ── DashSession ───────────────────────────────────────────────────────────────

/// The top-level coordinator for a DASH playback session.
///
/// # Usage
///
/// ```no_run
/// # use std::sync::Arc;
/// # use url::Url;
/// # use dash_rs::session::{DashSession, MediaType};
/// # use dash_rs::fetch::stub::StubSegmentFetcher;
/// # use dash_rs::media_source::stub::StubMediaSource;
/// # use dash_rs::abr::AbrMetrics;
/// # async fn example() -> Result<(), dash_rs::error::DashError> {
/// let mut media_source = StubMediaSource::new();
/// media_source.open();
///
/// let url = Url::parse("https://example.com/manifest.mpd").unwrap();
/// let mut session = DashSession::from_url(
///     &url,
///     StubSegmentFetcher,
///     media_source,
/// ).await?;
///
/// // Drive the fetch loop.
/// let metrics = session.current_metrics(MediaType::Video, 0.0);
/// let _ = session.fetch_next_segment(MediaType::Video, &metrics).await?;
/// # Ok(())
/// # }
/// ```
pub struct DashSession<F: SegmentFetcher, M: MediaSource> {
    mpd: MPD,
    base_url: Url,
    /// Alternative manifest URLs from `<Location>` elements in the MPD.
    ///
    /// Mirrors `LocationSelector.js` in dash.js.  When [`Self::refresh_manifest`]
    /// fails on `base_url`, each URL in this list is tried in order.  The first
    /// successful response updates `base_url` to that location URL so that all
    /// future refreshes use the working CDN.  Updated on every successful
    /// manifest refresh.
    location_urls: Vec<Url>,
    fetcher: Arc<F>,
    media_source: M,
    abr: AbrController,
    video_track: Option<TrackState>,
    audio_track: Option<TrackState>,
    /// Index of the MPD `Period` currently being fetched (0-based).
    ///
    /// Advances automatically at period boundaries inside
    /// [`Self::fetch_next_segment`].  See [`Self::period_count`] and
    /// [`Self::current_period_index`].
    current_period_index: usize,
    /// Buffer target in milliseconds.  Fetch is gated until estimated buffer
    /// level drops below this value — mirrors `bufferTimeDefault` (12 s) in
    /// dash.js's `ScheduleController`.
    buffer_target_ms: f64,
    /// Pluggable clock used for all time measurements.
    ///
    /// Defaults to [`SystemClock`] (backed by `std::time::Instant` and
    /// `SystemTime::now()`).  Inject a custom clock via
    /// [`Self::from_mpd_with_clock`] or [`Self::from_url_with_clock`] to run
    /// on platforms where those APIs are unavailable (e.g. WASM).
    clock: Arc<dyn Clock>,
    /// Clock reading (`clock.now_ms()`) when the pipeline transitioned to Playing.
    /// `None` until [`Self::notify_playing`] is called.
    playback_started_at: Option<f64>,
    /// True while the pipeline is in the Paused state.
    paused: bool,
    /// Clock reading (`clock.now_ms()`) when the pipeline entered Paused.
    paused_at: Option<f64>,
    /// Cumulative milliseconds spent in the Paused state since playback started.
    total_paused_ms: f64,
    /// When `true`, the fetch loop continues even while the pipeline is paused.
    schedule_while_paused: bool,
    /// How much buffer (in seconds) to preserve behind the current playback
    /// position when [`Self::prune_all_tracks`] is called.  Default 30 s.
    buffer_to_keep_s: f64,
    /// Optional maximum duration (in milliseconds) to wait for a single
    /// segment fetch before returning [`DashError::SegmentAbandoned`].
    abandon_timeout_ms: Option<u64>,
    /// Milliseconds between live-manifest refreshes.
    minimum_update_period_ms: Option<u64>,
    /// `clock.now_ms()` reading at the most recent manifest load or refresh.
    last_manifest_refresh: Option<f64>,
    /// Cache of initialisation segment bytes, keyed by `"{media_type}/{rep_id}"`.
    ///
    /// Populated by [`Self::fetch_init_segment`].  On a quality switch inside
    /// [`Self::fetch_next_segment`], the new representation's init segment is
    /// re-appended from this cache without a network round-trip (dash.js
    /// `InitCache.js` equivalent).  A cache miss triggers a live fetch that
    /// also populates the cache entry for future switches.
    init_cache: HashMap<String, bytes::Bytes>,
    /// Signed offset in seconds applied to the wall clock before live-edge
    /// segment calculations: `adjusted_now = clock.unix_s() + utc_clock_offset_s`.
    ///
    /// Set to `server_time − client_time` by [`Self::from_url`] after querying
    /// the first successful `UTCTiming` element in the manifest.  Stays `0.0`
    /// when no `UTCTiming` is present, all elements fail, or the session was
    /// constructed via [`Self::from_mpd`] (no network access allowed).
    utc_clock_offset_s: f64,
    /// Broadcast sender for in-band DASH events extracted from `emsg` boxes.
    ///
    /// Callers subscribe via [`Self::subscribe_events`].  If no subscribers
    /// are present, sends are silently dropped (the `async-broadcast` channel
    /// returns `Inactive` when there are zero receivers — we discard the error).
    event_tx: async_broadcast::Sender<DashEvent>,
    /// Keeps `inactive_receiver_count > 0` so that dropping the initial
    /// receiver (created by `broadcast()`) does not cause `close_channel()`
    /// to set `is_closed = true`.  Without this anchor, any subscriber
    /// obtained via `subscribe_events()` / `new_receiver()` would immediately
    /// see `TryRecvError::Closed` on the first call to `try_recv()`.
    _event_rx_anchor: async_broadcast::InactiveReceiver<DashEvent>,
    /// Optional maximum bitrate cap for video, in bits per second.
    ///
    /// When set, [`Self::fetch_next_segment`] clamps the ABR decision to the
    /// highest representation whose bandwidth is at or below this limit.
    video_max_bitrate_bps: Option<u64>,
    /// Optional maximum bitrate cap for audio, in bits per second.
    audio_max_bitrate_bps: Option<u64>,
    /// Time-based CDN blacklist.
    ///
    /// Mirrors `BlacklistController.js` in dash.js.  When a segment fetch fails
    /// for a `<BaseURL>` candidate, its service-location key is added here with
    /// an expiry of `now + cooldown` (default 600 s).  Subsequent fetches skip
    /// blacklisted candidates; if all candidates are blacklisted they are all
    /// retried anyway to avoid a permanent stall.
    cdn_blacklist: CdnBlacklist,
    /// Gap / stall controller.
    ///
    /// Mirrors `GapController.js` in dash.js.  Owned by the session so that
    /// the quality-switch guard (`on_track_switch_started/ended`) fires
    /// automatically at period boundaries.  Callers drive detection via
    /// [`Self::tick_gap`].
    gap_controller: GapController,
    /// DVR window depth in milliseconds, parsed from `MPD@timeShiftBufferDepth`.
    ///
    /// For live streams: segment numbers older than
    /// `live_edge − floor(tsbd / seg_dur)` are outside the sliding window and
    /// their CDN content may have been deleted.  `fetch_next_segment` advances
    /// `segment_number` past the floor when it detects this condition.
    ///
    /// `None` when the attribute is absent or for VOD streams.
    time_shift_buffer_depth_ms: Option<f64>,
    /// PTS remap offset applied by [`Self::apply_live_pts_remap`], in seconds.
    ///
    /// `0.0` until `apply_live_pts_remap` is called.  Once set, all track sinks
    /// have had their `timestampOffset` shifted by `-live_pts_remap_offset_s`,
    /// so epoch-scale fMP4 decode times land near presentation time 0 in the
    /// browser.  [`Self::live_presentation_start_s`] subtracts this offset so
    /// that the seek target returned to the caller is also near 0.
    ///
    /// GStreamer clients must NOT call `apply_live_pts_remap` — they apply a
    /// probe-based PTS subtraction via `GstMediaSource::set_live_mode(true)`.
    /// Double-applying would corrupt timing.
    live_pts_remap_offset_s: f64,
    /// Accumulated playback metrics: HTTP request log, representation-switch log,
    /// and buffer-level samples.  Populated automatically by
    /// [`Self::fetch_next_segment`].  Accessible via [`Self::metrics`].
    dash_metrics: DashMetrics,
}

// Re-export for convenience in examples.
pub use crate::session::MediaType as SessionMediaType;

// ── Private helpers (module-level) ────────────────────────────────────────────

/// Parse a DASH byte-range string of the form `"start-end"` (both inclusive)
/// into `(start, end)`.  Returns `None` when the string is absent or
/// malformed.
fn parse_byte_range(s: &str) -> Option<(u64, u64)> {
    let (start, end) = s.split_once('-')?;
    Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
}

impl<F: SegmentFetcher, M: MediaSource> DashSession<F, M> {
    // ── Constructors ──────────────────────────────────────────────────────────

    /// Fetch the manifest at `url`, parse it, initialise source buffers on
    /// `media_source`, and return a ready-to-use session.
    ///
    /// If the manifest contains `UTCTiming` elements, the first successful one
    /// is queried to compute a signed clock offset that corrects live-edge
    /// calculations for any skew between the client and server wall clocks.
    ///
    /// The session uses a [`SystemClock`] for all time measurements.  To
    /// inject a different clock (e.g. on WASM), use
    /// [`Self::from_url_with_clock`].
    pub async fn from_url(url: &Url, fetcher: F, media_source: M) -> Result<Self, DashError> {
        Self::from_url_with_clock(url, fetcher, media_source, SystemClock::arc()).await
    }

    /// Like [`Self::from_url`] but accepts a custom [`Clock`] implementation.
    ///
    /// Use this on platforms where `std::time::Instant` / `SystemTime::now()`
    /// are unavailable (e.g. `wasm32-unknown-unknown`).
    pub async fn from_url_with_clock(
        url: &Url,
        fetcher: F,
        media_source: M,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DashError> {
        let fetcher = Arc::new(fetcher);
        let loader = ManifestLoader::new(Arc::clone(&fetcher));
        let (mpd, base_url) = loader.load(url).await?;

        // Resolve any eager XLink references (xlink:actuate="onLoad") so that
        // the rest of session construction sees a fully self-contained MPD.
        let mpd = xlink::resolve_xlinks(mpd, &base_url, &*fetcher).await?;

        // Compute UTC clock offset before constructing the session so that
        // live-edge snapping inside `from_mpd` can use the corrected time.
        let client_now_s = clock.unix_s();
        let utc_offset = utc::resolve_utc_timing(&mpd, &fetcher, client_now_s).await;

        let mut session =
            Self::from_mpd_with_utc(mpd, base_url, fetcher, media_source, utc_offset, clock)?;
        session.last_manifest_refresh = Some(session.clock.now_ms());
        Ok(session)
    }

    /// Build a session from an already-parsed [`MPD`], without performing any
    /// network requests.
    ///
    /// The UTC clock offset is set to `0.0` — the system wall clock is used
    /// as-is for live-edge snapping.  Use [`Self::from_url`] to get automatic
    /// `UTCTiming` resolution.
    ///
    /// The session uses a [`SystemClock`] for all time measurements.  To
    /// inject a different clock, use [`Self::from_mpd_with_clock`].
    pub fn from_mpd(
        mpd: MPD,
        base_url: Url,
        fetcher: Arc<F>,
        media_source: M,
    ) -> Result<Self, DashError> {
        Self::from_mpd_with_utc(mpd, base_url, fetcher, media_source, 0.0, SystemClock::arc())
    }

    /// Like [`Self::from_mpd`] but accepts a custom [`Clock`] implementation.
    ///
    /// Use this on platforms where `std::time::Instant` / `SystemTime::now()`
    /// are unavailable (e.g. `wasm32-unknown-unknown`).
    pub fn from_mpd_with_clock(
        mpd: MPD,
        base_url: Url,
        fetcher: Arc<F>,
        media_source: M,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DashError> {
        Self::from_mpd_with_utc(mpd, base_url, fetcher, media_source, 0.0, clock)
    }

    /// Internal constructor shared by [`Self::from_url_with_clock`] and
    /// [`Self::from_mpd_with_clock`].
    fn from_mpd_with_utc(
        mpd: MPD,
        base_url: Url,
        fetcher: Arc<F>,
        mut media_source: M,
        utc_clock_offset_s: f64,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DashError> {
        if mpd.periods.is_empty() {
            return Err(DashError::EmptyManifest);
        }

        let period = &mpd.periods[0];
        let presentation_duration = mpd.mediaPresentationDuration;

        let video_adaptation = find_adaptation(period, &MediaType::Video);
        let audio_adaptation = find_adaptation(period, &MediaType::Audio);

        let mut video_track = if let Some(adaptation) = video_adaptation {
            let reinit = compute_track_reinit(adaptation, period, presentation_duration);
            if reinit.representations.is_empty() {
                None
            } else if !media_source.is_type_supported(&reinit.mime) {
                tracing::warn!(
                    mime = %reinit.mime,
                    "video MIME type not supported by this MediaSource — \
                     video track skipped (audio-only fallback)"
                );
                None
            } else {
                let sink_buf = media_source.add_source_buffer(&reinit.mime)?;
                let mut sink = SourceBufferSink::new(sink_buf);

                let effective_duration = period.duration.or(presentation_duration);
                let stream_info = StreamInfo {
                    start: period.start.map(|d| d.as_secs_f64()).unwrap_or(0.0),
                    duration: effective_duration.map(|d| d.as_secs_f64()),
                };
                sink.update_append_window(
                    &stream_info,
                    presentation_duration.map(|d| d.as_secs_f64()),
                );
                sink.set_timestamp_offset(reinit.timestamp_offset_s);

                Some(TrackState::new(
                    MediaType::Video,
                    reinit.representations,
                    sink,
                    reinit.start_number,
                    reinit.seg_dur_ms,
                    reinit.total_segments,
                    reinit.addressing_mode,
                    reinit.period_start_s,
                    reinit.timescale,
                    reinit.availability_time_offset_s,
                ))
            }
        } else {
            None
        };

        let mut audio_track = if let Some(adaptation) = audio_adaptation {
            let reinit = compute_track_reinit(adaptation, period, presentation_duration);
            if reinit.representations.is_empty() {
                None
            } else if !media_source.is_type_supported(&reinit.mime) {
                tracing::warn!(
                    mime = %reinit.mime,
                    "audio MIME type not supported by this MediaSource — \
                     audio track skipped"
                );
                None
            } else {
                let sink_buf = media_source.add_source_buffer(&reinit.mime)?;
                let mut sink = SourceBufferSink::new(sink_buf);

                let effective_duration = period.duration.or(presentation_duration);
                let stream_info = StreamInfo {
                    start: period.start.map(|d| d.as_secs_f64()).unwrap_or(0.0),
                    duration: effective_duration.map(|d| d.as_secs_f64()),
                };
                sink.update_append_window(
                    &stream_info,
                    presentation_duration.map(|d| d.as_secs_f64()),
                );
                sink.set_timestamp_offset(reinit.timestamp_offset_s);

                Some(TrackState::new(
                    MediaType::Audio,
                    reinit.representations,
                    sink,
                    reinit.start_number,
                    reinit.seg_dur_ms,
                    reinit.total_segments,
                    reinit.addressing_mode,
                    reinit.period_start_s,
                    reinit.timescale,
                    reinit.availability_time_offset_s,
                ))
            }
        } else {
            None
        };

        // Apply live-edge starting segment for dynamic streams.
        let is_dynamic = mpd.mpdtype.as_deref() == Some("dynamic");
        let ast_s: Option<f64> = mpd.availabilityStartTime;
        let spd_s: Option<f64> = mpd.suggestedPresentationDelay.map(|d| d.as_secs_f64());
        if is_dynamic {
            if let Some(ast) = ast_s {
                // Apply the UTC clock offset so that the live-edge calculation
                // uses the server's time rather than the (potentially skewed)
                // client wall clock.
                let now_s = clock.unix_s() + utc_clock_offset_s;
                if let Some(ref mut track) = video_track {
                    let seg = compute_live_edge_segment(
                        ast,
                        now_s,
                        track.segment_duration_ms / 1_000.0,
                        track.start_number,
                        spd_s,
                        track.availability_time_offset_s,
                    );
                    tracing::info!(
                        media_type = "video",
                        segment_number = seg,
                        utc_offset_s = utc_clock_offset_s,
                        "live stream: snapped to live-edge segment"
                    );
                    track.segment_number = seg;
                    track.initial_segment_number = seg;
                }
                if let Some(ref mut track) = audio_track {
                    let seg = compute_live_edge_segment(
                        ast,
                        now_s,
                        track.segment_duration_ms / 1_000.0,
                        track.start_number,
                        spd_s,
                        track.availability_time_offset_s,
                    );
                    tracing::info!(
                        media_type = "audio",
                        segment_number = seg,
                        utc_offset_s = utc_clock_offset_s,
                        "live stream: snapped to live-edge segment"
                    );
                    track.segment_number = seg;
                    track.initial_segment_number = seg;
                }
            }
        }

        // For SegmentTimeline ($Time$) tracks on live streams, snap
        // `next_segment_time` to the live-edge aligned position in the actual
        // SegmentTimeline.  Without this, `find_timeline_segment(None)` would
        // return the very first entry in the timeline (the beginning of the
        // time-shift window, potentially many minutes before the live edge),
        // causing the player to start from there instead of the live edge and
        // leaving the video element stalled for a long time.
        //
        // The live-edge segment number was computed by `compute_live_edge_segment`
        // using the $Number$ formula.  We convert it to timescale ticks and snap
        // it down to the nearest grid-aligned `t` in the SegmentTimeline using
        // `snap_to_timeline_grid`.
        if is_dynamic {
            if let Some(ref mut track) = video_track {
                if track.addressing_mode == AddressingMode::Timeline {
                    let adaptation = find_adaptation(period, &MediaType::Video);
                    if let Some(adaptation) = adaptation {
                        let repr_opt = adaptation.representations.first();
                        let tmpl = merge_segment_templates(repr_opt, adaptation, period);
                        if let Some(tl) = tmpl.as_ref().and_then(|t| t.SegmentTimeline.as_ref()) {
                            let seg_dur_ticks = (track.timescale as f64
                                * track.segment_duration_ms
                                / 1_000.0)
                                .round() as u64;
                            let target_ticks = track
                                .initial_segment_number
                                .saturating_sub(track.start_number)
                                * seg_dur_ticks;
                            match snap_to_timeline_grid(&tl.segments, target_ticks) {
                                Some(snapped) => {
                                    tracing::info!(
                                        media_type = "video",
                                        snapped_t = snapped,
                                        target_ticks,
                                        "live-edge snap: next_segment_time set for \
                                         SegmentTimeline track"
                                    );
                                    track.next_segment_time = Some(snapped);
                                }
                                None => {
                                    tracing::warn!(
                                        media_type = "video",
                                        target_ticks,
                                        "live-edge snap: could not snap next_segment_time \
                                         to SegmentTimeline grid — starting from beginning \
                                         of live window"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            if let Some(ref mut track) = audio_track {
                if track.addressing_mode == AddressingMode::Timeline {
                    let adaptation = find_adaptation(period, &MediaType::Audio);
                    if let Some(adaptation) = adaptation {
                        let repr_opt = adaptation.representations.first();
                        let tmpl = merge_segment_templates(repr_opt, adaptation, period);
                        if let Some(tl) = tmpl.as_ref().and_then(|t| t.SegmentTimeline.as_ref()) {
                            let seg_dur_ticks = (track.timescale as f64
                                * track.segment_duration_ms
                                / 1_000.0)
                                .round() as u64;
                            let target_ticks = track
                                .initial_segment_number
                                .saturating_sub(track.start_number)
                                * seg_dur_ticks;
                            match snap_to_timeline_grid(&tl.segments, target_ticks) {
                                Some(snapped) => {
                                    tracing::info!(
                                        media_type = "audio",
                                        snapped_t = snapped,
                                        target_ticks,
                                        "live-edge snap: next_segment_time set for \
                                         SegmentTimeline track"
                                    );
                                    track.next_segment_time = Some(snapped);
                                }
                                None => {
                                    tracing::warn!(
                                        media_type = "audio",
                                        target_ticks,
                                        "live-edge snap: could not snap next_segment_time \
                                         to SegmentTimeline grid — starting from beginning \
                                         of live window"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        let mpd_minimum_update_period_ms = mpd.minimumUpdatePeriod.map(|d| d.as_millis() as u64);

        // Parse MPD@timeShiftBufferDepth for live streams (ignored for VOD).
        let mpd_ref_tsbd: Option<f64> = if is_dynamic {
            mpd.timeShiftBufferDepth.map(|d| d.as_millis() as f64)
        } else {
            None
        };

        // Seed the buffer target from MPD@minBufferTime when present.
        // dash.js uses `Math.max(minBufferTime, bufferTimeDefault)` for VOD and
        // `bufferTimeDefault` for live; we follow the same policy.
        let buffer_target_ms = {
            let default_ms = 12_000.0_f64;
            if let Some(mbt) = mpd.minBufferTime {
                let mbt_ms = mbt.as_millis() as f64;
                if mpd.mpdtype.as_deref().is_none_or(|t| t != "dynamic") {
                    // VOD: use whichever is larger.
                    default_ms.max(mbt_ms)
                } else {
                    default_ms
                }
            } else {
                default_ms
            }
        };

        // Parse `<Location>` elements — alternative manifest URLs used as
        // fallback on refresh failure (mirrors `LocationSelector.js`).
        let location_urls: Vec<Url> = mpd
            .locations
            .iter()
            .filter_map(|loc| base_url.join(&loc.url).ok())
            .collect();
        if !location_urls.is_empty() {
            tracing::debug!(
                count = location_urls.len(),
                "manifest contains Location elements"
            );
        }

        let (event_tx, event_rx_anchor) = async_broadcast::broadcast(EVENT_CHANNEL_CAPACITY);
        let _event_rx_anchor = event_rx_anchor.deactivate();

        Ok(Self {
            mpd,
            base_url,
            location_urls,
            fetcher,
            media_source,
            abr: AbrController::default(),
            video_track,
            audio_track,
            current_period_index: 0,
            buffer_target_ms,
            clock,
            playback_started_at: None,
            paused: false,
            paused_at: None,
            total_paused_ms: 0.0,
            schedule_while_paused: false,
            buffer_to_keep_s: 30.0,
            abandon_timeout_ms: None,
            minimum_update_period_ms: mpd_minimum_update_period_ms,
            last_manifest_refresh: None,
            init_cache: HashMap::new(),
            utc_clock_offset_s,
            event_tx,
            _event_rx_anchor,
            video_max_bitrate_bps: None,
            audio_max_bitrate_bps: None,
            cdn_blacklist: CdnBlacklist::new(),
             gap_controller: GapController::new(),
             time_shift_buffer_depth_ms: mpd_ref_tsbd,
             live_pts_remap_offset_s: 0.0,
             dash_metrics: DashMetrics::new(),
         })
    }

    // ── Introspection ─────────────────────────────────────────────────────────

    /// The parsed MPD manifest.
    pub fn mpd(&self) -> &MPD {
        &self.mpd
    }

    /// Returns `true` when the manifest contains a video `AdaptationSet` with
    /// at least one representation.
    ///
    /// Use this to gate video-specific operations (e.g.
    /// [`Self::fetch_next_segment`] with [`MediaType::Video`]) so that
    /// audio-only streams do not produce a
    /// [`DashError::NoRepresentations`] error.
    pub fn has_video(&self) -> bool {
        self.video_track.is_some()
    }

    /// Returns `true` when the manifest contains an audio `AdaptationSet` with
    /// at least one representation.
    ///
    /// Use this to gate audio-specific operations on streams that may be
    /// video-only.
    pub fn has_audio(&self) -> bool {
        self.audio_track.is_some()
    }

    /// The URL that the manifest was last loaded from (or successfully refreshed from).
    ///
    /// Updated to the responding `<Location>` URL when a refresh falls back to
    /// one of the alternative locations.
    pub fn manifest_url(&self) -> &Url {
        &self.base_url
    }

    /// Alternative manifest URLs parsed from `<Location>` elements.
    ///
    /// On manifest refresh failure, these are tried in order before the error
    /// is returned.  Updated on every successful manifest refresh.
    pub fn location_urls(&self) -> &[Url] {
        &self.location_urls
    }

    /// Cooldown duration applied to a failing CDN (default 600 s).
    ///
    /// Mirrors `BlacklistController.BLACKLIST_DURATION` in dash.js.
    pub fn cdn_blacklist_cooldown_ms(&self) -> u64 {
        self.cdn_blacklist.cooldown().as_millis() as u64
    }

    /// Override the CDN blacklist cooldown.
    ///
    /// Set to `0` to disable blacklisting (every segment fetch retries all
    /// candidates, which was the previous behaviour).
    pub fn set_cdn_blacklist_cooldown_ms(&mut self, ms: u64) {
        self.cdn_blacklist
            .set_cooldown(core::time::Duration::from_millis(ms));
    }

    // ── Gap / stall control ───────────────────────────────────────────────────

    /// Advance the internal gap / stall detector one tick.
    ///
    /// Call this periodically (e.g. every 200 ms) from your playback loop,
    /// supplying the renderer's current position and the union of all
    /// `SourceBuffer::buffered()` ranges.  When the controller decides a seek
    /// is needed it returns a [`GapSeekTarget`]; otherwise returns `None`.
    ///
    /// When a seek is returned, the caller should:
    /// 1. Seek the renderer to `target.position_s`.
    /// 2. Call [`Self::seek_to`] with the same value — this automatically
    ///    resets the gap controller via [`GapController::on_seek`].
    /// 3. Call [`Self::notify_playing`] to restart the buffer-level clock.
    ///
    /// This mirrors `GapController.js::_jumpGap` / `_checkStall` in dash.js.
    pub fn tick_gap(
        &mut self,
        position_s: f64,
        buffered: &[crate::media_source::TimeRange],
        is_paused: bool,
    ) -> Option<GapSeekTarget> {
        self.gap_controller.tick(position_s, buffered, is_paused)
    }

    /// Read-only access to the gap controller settings.
    pub fn gap_settings(&self) -> &GapControllerSettings {
        self.gap_controller.settings()
    }

    /// Replace the gap controller settings.
    ///
    /// # Example
    /// ```rust,ignore
    /// let mut settings = session.gap_settings().clone();
    /// settings.jump_large_gaps = false;
    /// session.set_gap_settings(settings);
    /// ```
    pub fn set_gap_settings(&mut self, settings: GapControllerSettings) {
        self.gap_controller.set_settings(settings);
    }

    /// `MPD@minBufferTime` in milliseconds, or `None` when the attribute is absent.
    ///
    /// DASH spec §5.3.1.2: the minimum amount of data the client should buffer
    /// before starting playback.  [`Self::buffer_target_ms`] is seeded from
    /// this value at session construction (VOD only; capped to 12 s minimum).
    pub fn min_buffer_time_ms(&self) -> Option<f64> {
        self.mpd.minBufferTime.map(|d| d.as_millis() as f64)
    }

    /// DVR window depth in milliseconds, parsed from `MPD@timeShiftBufferDepth`.
    ///
    /// `None` when the attribute is absent or for VOD streams.
    /// When present, `fetch_next_segment` silently advances `segment_number`
    /// past segments that have expired from the live DVR window rather than
    /// repeatedly failing with `AllBaseUrlsFailed`.
    pub fn time_shift_buffer_depth_ms(&self) -> Option<f64> {
        self.time_shift_buffer_depth_ms
    }

    /// The number of `Period` elements in the manifest.
    pub fn period_count(&self) -> usize {
        self.mpd.periods.len()
    }

    /// Zero-based index of the `Period` currently being fetched.
    ///
    /// Starts at `0` and advances automatically at period boundaries inside
    /// [`Self::fetch_next_segment`].
    pub fn current_period_index(&self) -> usize {
        self.current_period_index
    }

    /// Signed clock offset applied to live-edge calculations:
    /// `adjusted_now = clock.unix_s() + utc_clock_offset_s`.
    ///
    /// Set by [`Self::from_url`] after querying the first successful
    /// `UTCTiming` element in the manifest.  `0.0` when no `UTCTiming` is
    /// present or the session was built via [`Self::from_mpd`].
    pub fn utc_clock_offset_s(&self) -> f64 {
        self.utc_clock_offset_s
    }

    /// Target playout latency in milliseconds from the first
    /// `<ServiceDescription><Latency@target>` element, or `None` when the
    /// manifest does not declare one.
    ///
    /// This corresponds to `ServiceDescriptionController.js` in dash.js.
    /// Expose the value to the application so it can configure a low-latency
    /// catch-up controller (see [`Self::set_buffer_target_ms`]).
    ///
    /// Note: `@target` is in seconds in the MPEG-DASH spec; this method
    /// converts to milliseconds for consistency with the rest of the API.
    pub fn target_latency_ms(&self) -> Option<f64> {
        self.mpd
            .ServiceDescription
            .iter()
            .flat_map(|sd| sd.Latency.iter())
            .find_map(|l| l.target.map(|t| t * 1_000.0))
    }

    /// Recommended playback-rate range `(min, max)` from the first
    /// `<ServiceDescription><PlaybackRate>` element that declares both
    /// attributes.  Returns `None` when the manifest does not declare one.
    ///
    /// The application can use these bounds to implement catch-up / slow-down
    /// rate control when the actual latency deviates from
    /// [`Self::target_latency_ms`].
    pub fn playback_rate_range(&self) -> Option<(f64, f64)> {
        self.mpd
            .ServiceDescription
            .iter()
            .flat_map(|sd| sd.PlaybackRate.iter())
            .find_map(|pr| Some((pr.min?, pr.max?)))
    }

    /// Subscribe to in-band DASH events extracted from `emsg` boxes.
    ///
    /// Call this **before** starting the fetch loop so that no events are
    /// missed.  Events are broadcast from within [`Self::fetch_next_segment`]
    /// immediately after the segment bytes are received and parsed.
    ///
    /// The returned [`async_broadcast::Receiver`] is a multi-producer,
    /// multi-consumer channel — multiple subscribers each receive every event
    /// independently.  If a subscriber's internal buffer fills up (capacity
    /// [`EVENT_CHANNEL_CAPACITY`] = 64), the oldest events are overwritten and
    /// the next `recv()` call will return
    /// [`async_broadcast::RecvError::Overflowed`].
    pub fn subscribe_events(&self) -> async_broadcast::Receiver<DashEvent> {
        self.event_tx.new_receiver()
    }

    /// Shared reference to the underlying [`MediaSource`].
    pub fn media_source(&self) -> &M {
        &self.media_source
    }

    /// Exclusive reference to the underlying [`MediaSource`].
    pub fn media_source_mut(&mut self) -> &mut M {
        &mut self.media_source
    }

    /// Available video representations, sorted ascending by bandwidth.
    pub fn video_representations(&self) -> &[RepresentationInfo] {
        self.video_track
            .as_ref()
            .map(|t| t.representations.as_slice())
            .unwrap_or(&[])
    }

    /// Available audio representations, sorted ascending by bandwidth.
    pub fn audio_representations(&self) -> &[RepresentationInfo] {
        self.audio_track
            .as_ref()
            .map(|t| t.representations.as_slice())
            .unwrap_or(&[])
    }

    /// Build an [`AbrMetrics`] snapshot for the given media type using the
    /// current session state.
    ///
    /// `throughput_bps` is the per-track exponential moving average of
    /// measured download throughput, updated after every successful
    /// [`Self::fetch_next_segment`] call.  It is `0.0` before the first
    /// segment has been fetched.
    ///
    /// `latency_ms` is the transfer duration of the most recent fetch (total
    /// bytes-on-the-wire time, not a pure round-trip time).
    ///
    /// `buffer_level_ms` is computed as `(buffered_end − position_s) * 1000`,
    /// clamped to `0`.  Pass the current renderer position in seconds (e.g.
    /// from `query_position` in GStreamer or `currentTime` in MSE).  Passing
    /// `0.0` gives the absolute buffer-end time, which overestimates the
    /// available buffer after the first few segments.
    pub fn current_metrics(&self, media_type: MediaType, position_s: f64) -> AbrMetrics {
        let track = match media_type {
            MediaType::Video => self.video_track.as_ref(),
            MediaType::Audio => self.audio_track.as_ref(),
        };

        let (buffer_level_ms, current_index, throughput_bps, latency_ms) =
            if let Some(t) = track {
                let buffered = t.sink.buffered();
                let level = buffered
                    .last()
                    .map(|r| (r.end - position_s).max(0.0) * 1_000.0)
                    .unwrap_or(0.0);
                (level, Some(t.current_index), t.throughput_tracker.estimate(), t.last_transfer_ms)
            } else {
                (0.0, None, 0.0, 0.0)
            };

        AbrMetrics {
            throughput_bps,
            buffer_level_ms,
            latency_ms,
            current_index,
            dropped_frames: None,
            total_decoded_frames: None,
            effective_download_ratio: None,
        }
    }

    // ── Buffer scheduling ─────────────────────────────────────────────────────

    /// Record that the pipeline has entered Playing state.
    pub fn notify_playing(&mut self) {
        if self.paused {
            if let Some(paused_at) = self.paused_at.take() {
                self.total_paused_ms += self.clock.now_ms() - paused_at;
            }
            self.paused = false;
        } else {
            self.playback_started_at = Some(self.clock.now_ms());
            self.total_paused_ms = 0.0;
        }
    }

    /// Record that the pipeline has entered Paused state.
    pub fn notify_paused(&mut self) {
        if !self.paused {
            self.paused = true;
            self.paused_at = Some(self.clock.now_ms());
        }
    }

    /// Whether the session is currently tracking a paused pipeline.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Whether the fetch loop should continue to run while the pipeline is paused.
    pub fn schedule_while_paused(&self) -> bool {
        self.schedule_while_paused
    }

    /// Set whether the fetch loop continues during pause.
    pub fn set_schedule_while_paused(&mut self, v: bool) {
        self.schedule_while_paused = v;
    }

    /// Synchronise the session's pause state from an external boolean signal.
    ///
    /// This is a convenience wrapper around [`Self::notify_paused`] and
    /// [`Self::notify_playing`] that eliminates the `was_paused` tracking
    /// variable callers would otherwise need.  It is safe to call on every
    /// loop iteration — both `notify_paused` and `notify_playing` are
    /// idempotent when the state has not changed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // At the top of each fetch-loop iteration:
    /// session.sync_pause_state(renderer.is_paused());
    /// ```
    pub fn sync_pause_state(&mut self, is_paused: bool) {
        if is_paused && !self.paused {
            self.notify_paused();
        } else if !is_paused && self.paused {
            self.notify_playing();
        }
    }

    /// Wait until the buffer for `media_type` has drained below
    /// [`Self::buffer_target_ms`].
    ///
    /// This is the library-side implementation of the dash.js
    /// `ScheduleController` buffer-gate loop.  It sleeps in
    /// [`crate::clock::Clock::set_timeout`] increments of 500 ms (matching
    /// `dash.js` `scheduleTimeout`) and calls [`Self::sync_pause_state`] after
    /// each wake so that a resume while sleeping correctly unfreezes
    /// [`Self::estimated_buffer_level_ms`].
    ///
    /// Returns immediately if the buffer is already below target.
    ///
    /// # Arguments
    ///
    /// * `media_type` — the track whose buffer level gates fetching.
    /// * `is_paused`  — closure that returns the current renderer pause state.
    ///   Queried after every sleep tick so that pause/resume transitions are
    ///   handled without the caller needing a `was_paused` variable.
    ///
    /// # Example
    ///
    /// ```ignore
    /// session.wait_for_buffer_capacity(&MediaType::Video, || renderer.is_paused()).await;
    /// session.fetch_next_segment(MediaType::Video, &metrics).await?;
    /// ```
    pub async fn wait_for_buffer_capacity(
        &mut self,
        media_type: &MediaType,
        is_paused: impl Fn() -> bool,
    ) {
        // Clone the Arc so the await point on set_timeout does not hold a
        // borrow on `self`, allowing sync_pause_state to take &mut self.
        let clock = Arc::clone(&self.clock);
        while self.estimated_buffer_level_ms(media_type) >= self.buffer_target_ms() {
            clock.set_timeout(500.0).await;
            self.sync_pause_state(is_paused());
        }
    }

    /// Return a clone of the session's [`Arc`]`<dyn `[`Clock`]`>`.
    ///
    /// Useful when a caller needs to drive its own timed loop (e.g. a gate
    /// with a non-standard threshold) using the same platform sleep primitive
    /// as the library, without hardcoding `tokio::time::sleep` or any other
    /// runtime-specific API.
    ///
    /// ```ignore
    /// let clock = session.clock_arc();
    /// loop {
    ///     if buf < custom_target { break; }
    ///     clock.set_timeout(500.0).await;
    ///     session.sync_pause_state(renderer.is_paused());
    /// }
    /// ```
    pub fn clock_arc(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    // ── High-level playback loop ──────────────────────────────────────────────

    /// Drive the full DASH segment fetch loop until end-of-stream (or until
    /// [`PlaybackDriver::should_continue`] returns `false`).
    ///
    /// This is the recommended entry point for applications that want the
    /// library to handle the fetch-loop bookkeeping.  Per-iteration callbacks
    /// on [`PlaybackDriver`] let callers inject platform-specific behaviour
    /// (renderer position, pause state, gap-fill seek, throughput measurement,
    /// progress reporting) without duplicating the loop structure.
    ///
    /// # What `run` does each iteration
    ///
    /// 1. Calls [`PlaybackDriver::sync_pause_state`][Self::sync_pause_state]
    ///    so buffer-level estimates stay accurate during pauses.
    /// 2. Calls [`PlaybackDriver::on_iteration_start`] with the session's
    ///    current estimated playback position.
    /// 3. Optionally calls [`prune_all_tracks`][Self::prune_all_tracks] when
    ///    [`PlaybackDriver::enable_pruning`] returns `true`.
    /// 4. Ticks the gap controller and calls
    ///    [`PlaybackDriver::on_gap_seek`] when a seek is recommended.
    /// 5. Waits for the primary (video-preferred) buffer to drain below
    ///    [`buffer_target_ms`][Self::buffer_target_ms].
    /// 6. Fetches the next video segment; handles `EndOfStream` gracefully.
    /// 7. Waits for the audio buffer to drain below the threshold returned by
    ///    [`PlaybackDriver::audio_gate_target_ms`].
    /// 8. Fetches the next audio segment; handles `EndOfStream` gracefully.
    /// 9. Calls [`PlaybackDriver::on_iteration`] with an [`IterationSummary`].
    ///
    /// After the loop, [`end_of_stream`][Self::end_of_stream] is called
    /// regardless of whether the exit was via EOS or `should_continue`.
    ///
    /// [`IterationSummary`]: driver::IterationSummary
    /// [`PlaybackDriver`]: driver::PlaybackDriver
    pub async fn run<D: driver::PlaybackDriver>(
        &mut self,
        drv: &mut D,
    ) -> Result<(), DashError> {
        use driver::IterationSummary;

        let gate = if self.has_video() { MediaType::Video } else { MediaType::Audio };
        let mut video_done = !self.has_video();
        let mut audio_done = !self.has_audio();

        self.notify_playing();

        loop {
            if video_done && audio_done {
                break;
            }
            if !drv.should_continue() {
                break;
            }

            // ── 1. Pause sync ────────────────────────────────────────────────
            self.sync_pause_state(drv.is_paused());

            // ── 2. Iteration start ───────────────────────────────────────────
            let session_pos = self.estimated_playback_position_s();
            drv.on_iteration_start(session_pos);

            // ── 3. Optional pruning ──────────────────────────────────────────
            if drv.enable_pruning() {
                self.prune_all_tracks(session_pos, self.buffer_to_keep_s()).await?;
            }

            // ── 4. Gap controller ────────────────────────────────────────────
            {
                let pos = drv.position_s();
                let buffered = self.buffered(gate.clone());
                if let Some(target) = self.tick_gap(pos, &buffered, drv.is_paused()) {
                    tracing::info!(
                        pos,
                        seek_to = target.position_s,
                        reason  = ?target.reason,
                        "gap controller: seeking"
                    );
                    self.seek_to(target.position_s);
                    drv.on_gap_seek(target).await;
                    self.notify_playing();
                }
            }

            // ── 5. Video buffer gate ─────────────────────────────────────────
            self.wait_for_buffer_capacity(&gate, || drv.is_paused()).await;

            // ── 6. Video fetch ───────────────────────────────────────────────
            let pos      = drv.position_s();
            let bps      = drv.throughput_override_bps();
            let mut video_decision = None;

            if !video_done {
                let mut m = self.current_metrics(MediaType::Video, pos);
                if let Some(b) = bps { m.throughput_bps = b; }
                match self.fetch_next_segment(MediaType::Video, &m).await {
                    Ok(d) => {
                        tracing::debug!(index = d.index, reason = ?d.reason, "video segment fetched");
                        video_decision = Some(d);
                    }
                    Err(DashError::EndOfStream { .. }) => {
                        tracing::info!("video: end of stream");
                        video_done = true;
                    }
                    Err(e) => return Err(e),
                }
            }

            // ── 7. Audio buffer gate (custom target via driver) ──────────────
            if !audio_done {
                let vbuf = self.estimated_buffer_level_ms(&MediaType::Video);
                let audio_target = drv.audio_gate_target_ms(vbuf, self.buffer_target_ms());
                let clock = Arc::clone(&self.clock);
                while self.estimated_buffer_level_ms(&MediaType::Audio) >= audio_target {
                    tracing::debug!(
                        buf_ms = self.estimated_buffer_level_ms(&MediaType::Audio) as u64,
                        target_ms = audio_target as u64,
                        "audio buffer gate: waiting"
                    );
                    clock.set_timeout(500.0).await;
                    self.sync_pause_state(drv.is_paused());
                }
            }

            // ── 8. Audio fetch ───────────────────────────────────────────────
            let mut audio_decision = None;

            if !audio_done {
                let mut m = self.current_metrics(MediaType::Audio, pos);
                if let Some(b) = bps { m.throughput_bps = b; }
                match self.fetch_next_segment(MediaType::Audio, &m).await {
                    Ok(d) => {
                        tracing::debug!(index = d.index, reason = ?d.reason, "audio segment fetched");
                        audio_decision = Some(d);
                    }
                    Err(DashError::EndOfStream { .. }) => {
                        tracing::info!("audio: end of stream");
                        audio_done = true;
                    }
                    Err(e) => return Err(e),
                }
            }

            // ── 9. Progress callback ─────────────────────────────────────────
            let (last_url, last_tms, last_tbps) = self
                .metrics()
                .last_http_request()
                .map(|r| {
                    (r.url.clone(), r.transfer_duration_ms, r.throughput_bps.unwrap_or(0.0))
                })
                .unwrap_or_default();

            let video_bw = video_decision
                .as_ref()
                .and_then(|d| self.video_representations().get(d.index))
                .map(|r| r.bandwidth)
                .unwrap_or(0);
            let audio_bw = audio_decision
                .as_ref()
                .and_then(|d| self.audio_representations().get(d.index))
                .map(|r| r.bandwidth)
                .unwrap_or(0);

            drv.on_iteration(&IterationSummary {
                position_s:          session_pos,
                video_decision:      video_decision.as_ref(),
                audio_decision:      audio_decision.as_ref(),
                video_bandwidth:     video_bw,
                audio_bandwidth:     audio_bw,
                video_buffer_ms:     self.estimated_buffer_level_ms(&MediaType::Video),
                audio_buffer_ms:     self.estimated_buffer_level_ms(&MediaType::Audio),
                buffer_target_ms:    self.buffer_target_ms(),
                http_request_count:  self.metrics().http_requests.len(),
                switch_count:        self.metrics().representation_switches.len(),
                last_transfer_url:   last_url,
                last_transfer_ms:    last_tms,
                last_throughput_bps: last_tbps,
            });
        }

        self.end_of_stream().await?;
        Ok(())
    }

    /// Estimated buffer level ahead of the current playback position, in
    /// milliseconds.
    pub fn estimated_buffer_level_ms(&self, media_type: &MediaType) -> f64 {
        let track = match self.track(media_type) {
            Ok(t) => t,
            Err(_) => return 0.0,
        };
        let segments_fetched = track
            .segment_number
            .saturating_sub(track.initial_segment_number);
        let pushed_ms = segments_fetched as f64 * track.segment_duration_ms;
        let elapsed_ms = self
            .playback_started_at
            .map(|started_ms| {
                let total_elapsed = self.clock.now_ms() - started_ms;
                let current_pause_ms = if self.paused {
                    self.paused_at
                        .map(|pa_ms| self.clock.now_ms() - pa_ms)
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                (total_elapsed - self.total_paused_ms - current_pause_ms).max(0.0)
            })
            .unwrap_or(0.0);
        (pushed_ms - elapsed_ms).max(0.0)
    }

    /// Estimated wall-clock seconds elapsed since playback started, excluding
    /// time spent paused.
    pub fn estimated_playback_position_s(&self) -> f64 {
        self.playback_started_at
            .map(|started_ms| {
                let total_elapsed = self.clock.now_ms() - started_ms;
                let current_pause_ms = if self.paused {
                    self.paused_at
                        .map(|pa_ms| self.clock.now_ms() - pa_ms)
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                ((total_elapsed - self.total_paused_ms - current_pause_ms) / 1_000.0).max(0.0)
            })
            .unwrap_or(0.0)
    }

    /// The buffer fill target in milliseconds (default 12 000 ms).
    pub fn buffer_target_ms(&self) -> f64 {
        self.buffer_target_ms
    }

    /// Override the buffer fill target.
    pub fn set_buffer_target_ms(&mut self, ms: f64) {
        self.buffer_target_ms = ms;
    }

    /// How much buffer to keep behind the playback position when pruning.
    pub fn buffer_to_keep_s(&self) -> f64 {
        self.buffer_to_keep_s
    }

    /// Override the behind-playback buffer retention window.
    pub fn set_buffer_to_keep_s(&mut self, s: f64) {
        self.buffer_to_keep_s = s;
    }

    /// The segment-fetch abandon timeout in milliseconds, or `None` if disabled.
    pub fn abandon_timeout_ms(&self) -> Option<u64> {
        self.abandon_timeout_ms
    }

    /// Set (or clear) the per-segment fetch timeout.
    pub fn set_abandon_timeout_ms(&mut self, ms: Option<u64>) {
        self.abandon_timeout_ms = ms;
    }

    // ── Bitrate cap ───────────────────────────────────────────────────────────

    /// Constrain ABR quality selection to representations with
    /// `bandwidth ≤ bps`.
    ///
    /// After this call, [`Self::fetch_next_segment`] will never select a
    /// representation above `bps` bits/s for `media_type`, regardless of the
    /// ABR algorithm's unconstrained choice.  The lowest representation is
    /// always eligible even when its bandwidth exceeds the cap.
    ///
    /// Mirrors `AbrController.setMaxAllowedBitrateFor()` in dash.js.
    pub fn set_max_bitrate(&mut self, media_type: MediaType, bps: u64) {
        match media_type {
            MediaType::Video => self.video_max_bitrate_bps = Some(bps),
            MediaType::Audio => self.audio_max_bitrate_bps = Some(bps),
        }
    }

    /// Remove a previously set bitrate cap for `media_type`.
    pub fn clear_max_bitrate(&mut self, media_type: MediaType) {
        match media_type {
            MediaType::Video => self.video_max_bitrate_bps = None,
            MediaType::Audio => self.audio_max_bitrate_bps = None,
        }
    }

    /// The active bitrate cap for `media_type`, or `None` when uncapped.
    pub fn max_bitrate(&self, media_type: &MediaType) -> Option<u64> {
        match media_type {
            MediaType::Video => self.video_max_bitrate_bps,
            MediaType::Audio => self.audio_max_bitrate_bps,
        }
    }

    // ── Buffer pruning ────────────────────────────────────────────────────────

    /// Remove buffered data older than `current_time_s − buffer_to_keep_behind_s`
    /// from all tracks.
    pub async fn prune_all_tracks(
        &mut self,
        current_time_s: f64,
        buffer_to_keep_behind_s: f64,
    ) -> Result<(), DashError> {
        let prune_end = (current_time_s - buffer_to_keep_behind_s).max(0.0);
        if prune_end <= 0.0 {
            return Ok(());
        }

        if let Some(track) = self.video_track.as_mut() {
            Self::prune_sink(&mut track.sink, prune_end).await?;
        }
        if let Some(track) = self.audio_track.as_mut() {
            Self::prune_sink(&mut track.sink, prune_end).await?;
        }
        Ok(())
    }

    async fn prune_sink(sink: &mut SourceBufferSink, prune_end: f64) -> Result<(), DashError> {
        let ranges: Vec<_> = sink.buffered();
        for range in ranges {
            if range.end <= prune_end {
                sink.remove(range.start, range.end).await?;
            } else if range.start < prune_end {
                sink.remove(range.start, prune_end).await?;
            }
        }
        Ok(())
    }

    // ── Seeking ───────────────────────────────────────────────────────────────

    /// Reposition both tracks to the segment containing `position_s`.
    ///
    /// This is the counterpart of [`GapController::tick`]: when the gap
    /// controller returns a [`GapSeekTarget`], pass `target.position_s` to
    /// this method (and seek the renderer separately).
    ///
    /// # What it does
    ///
    /// For each track the segment number is recomputed as:
    ///
    /// ```text
    /// new_segment = start_number + floor(position_s / segment_duration_s)
    /// ```
    ///
    /// clamped to `[start_number, start_number + total_segments − 1]`.
    ///
    /// `initial_segment_number` is set to the new segment number so that
    /// [`Self::estimated_buffer_level_ms`] counts from zero at the new
    /// position.  For `SegmentTimeline` (`$Time$`) tracks `next_segment_time`
    /// is reset to `None` so the timeline walk restarts from the correct
    /// entry.  For `SegmentBase` tracks the `sidx` index is cleared and
    /// will be re-fetched on the next [`Self::fetch_next_segment`] call.
    ///
    /// # After seeking
    ///
    /// Call [`Self::notify_playing`] once the renderer has completed its seek
    /// so that the wall-clock used by `estimated_buffer_level_ms` and
    /// `estimated_playback_position_s` restarts from the new position.
    ///
    /// # Returns
    ///
    /// The snapped position in seconds (start of the segment the seek landed
    /// in).  This may differ slightly from `position_s` due to segment
    /// boundaries.
    pub fn seek_to(&mut self, position_s: f64) -> f64 {
        let position_s = position_s.max(0.0);
        // When apply_live_pts_remap() has been called, `position_s` is in
        // remapped (≈0-based) presentation time.  The segment-number formula
        // expects un-remapped time, so we add the stored offset back before
        // dividing.  When no remap has been applied the offset is 0.0 and
        // this is a no-op (backward-compatible).
        let remap_offset = self.live_pts_remap_offset_s;

        let mut snapped_s: f64 = position_s;

        for track in [self.video_track.as_mut(), self.audio_track.as_mut()]
            .into_iter()
            .flatten()
        {
        let seg_dur_s = track.segment_duration_ms / 1_000.0;
        // Map the presentation-timeline position into a segment index.
        // DASH spec: presentation_time(N) = (N − start_number) × seg_dur_s + Period@start
        // → N = start_number + ⌊(position_s + remap_offset − Period@start) / seg_dur_s⌋
        //
        // The remap_offset converts from remapped time (video.currentTime space)
        // back to un-remapped DTS time so the formula gives the correct segment.
        let adjusted_pos = (position_s + remap_offset - track.period_start_s).max(0.0);
        let offset_segs = if seg_dur_s > 0.0 {
            (adjusted_pos / seg_dur_s).floor() as u64
        } else {
            0
        };

            let new_seg = track.start_number + offset_segs;

            // Clamp to declared total, if known.
            let new_seg = if let Some(total) = track.total_segments {
                new_seg.min(track.start_number + total.saturating_sub(1))
            } else {
                new_seg
            };

            // For SegmentList the start_number is the raw array index base
            // (usually 0); arithmetic is the same.
            let new_seg = new_seg.max(track.start_number);

            // Reset per-addressing-mode state.
            // For SegmentTimeline ($Time$) tracks, compute the target time in
            // timescale ticks and set next_segment_time directly — this lets
            // find_timeline_segment snap to the nearest valid entry instead of
            // restarting from the first entry (which is wrong for live streams
            // with large absolute tick values).
            // Add remap_offset so the ticks land in the correct DTS-space position.
            if track.addressing_mode == AddressingMode::Timeline {
                let target_ticks = ((position_s + remap_offset - track.period_start_s).max(0.0)
                    * track.timescale as f64)
                    .round() as u64;
                track.next_segment_time = Some(target_ticks);
            } else {
                track.next_segment_time = None;
            }
            if matches!(track.addressing_mode, AddressingMode::Base { .. }) {
                track.segment_byte_ranges = None; // sidx will be re-fetched
            }

            track.segment_number = new_seg;
            // Reset the buffer-level baseline so estimates count from zero at
            // the new position.
            track.initial_segment_number = new_seg;

            // Return the snapped position in the same coordinate space that
            // position_s was given in (remapped if remap was applied, un-remapped
            // otherwise) so the caller can use the value directly.
            let snapped_offset = new_seg.saturating_sub(track.start_number);
            snapped_s = snapped_offset as f64 * seg_dur_s + track.period_start_s - remap_offset;
        }

        tracing::info!(
            requested_s = position_s,
            snapped_s,
            "DashSession::seek_to"
        );
        self.gap_controller.on_seek();
        snapped_s
    }

    // ── Live stream ───────────────────────────────────────────────────────────

    /// Returns `true` when the manifest describes a live/dynamic stream.
    pub fn is_live(&self) -> bool {
        self.mpd.mpdtype.as_deref() == Some("dynamic")
    }

    /// Returns the DASH presentation-timeline position (in seconds) at which
    /// the first live segment will be buffered in the MSE `SourceBuffer`.
    ///
    /// This matches the dash.js approach: `timestampOffset` is set to
    /// `Period@start − presentationTimeOffset/timescale`, so fMP4 decode times
    /// are NOT remapped — they retain their native (potentially large) values.
    /// The caller must seek the media element to this position so that it finds
    /// the buffered content.
    ///
    /// For `SegmentTimeline` (`$Time$`) tracks, the snapped `next_segment_time`
    /// tick value (set during live-edge snapping) is divided by the timescale to
    /// give the exact presentation time of the first segment.  This is more
    /// accurate than the number-based formula because it uses the actual
    /// SegmentTimeline grid rather than an approximation from `initial_segment_number`.
    ///
    /// For `$Number$` tracks the formula is:
    /// `(initial_segment_number − start_number) × seg_dur_s`.
    ///
    /// If [`Self::apply_live_pts_remap`] has been called, the stored remap
    /// offset is subtracted from the native start position so that the returned
    /// value reflects where the content actually lands in the browser (≈ 0 for
    /// epoch-scale streams after remapping).
    ///
    /// Returns `0.0` for VOD streams or when no tracks are present.
    pub fn live_presentation_start_s(&self) -> f64 {
        if !self.is_live() {
            return 0.0;
        }
        let track = self.video_track.as_ref().or(self.audio_track.as_ref());
        let native_start = track.map_or(0.0, |t| {
            // For $Time$ tracks, prefer the snapped timeline tick value — it is
            // aligned to the actual segment grid and avoids the rounding mismatch
            // between the $Number$ live-edge formula and the SegmentTimeline's `t`
            // values.  Without this, the seek target can be off by up to one
            // segment duration, keeping the browser spinner active until enough
            // segments are appended to cover the gap.
            if t.addressing_mode == AddressingMode::Timeline {
                if let Some(ticks) = t.next_segment_time {
                    return ticks as f64 / t.timescale as f64;
                }
            }
            // Fallback ($Number$ tracks or Timeline tracks where snapping failed).
            let seg_dur_s = t.segment_duration_ms / 1_000.0;
            t.initial_segment_number.saturating_sub(t.start_number) as f64 * seg_dur_s
        });
        // Subtract any remap offset applied by apply_live_pts_remap() so the
        // caller receives the position where content actually lands in the
        // browser (≈ 0 for epoch-scale streams after remapping).
        (native_start - self.live_pts_remap_offset_s).max(0.0)
    }

    /// Remap epoch-scale fMP4 presentation timestamps to start near zero.
    ///
    /// On live streams (e.g. BBC IPTV) fMP4 decode timestamps are large
    /// absolute values — sometimes tens of billions of timescale ticks.  The
    /// dash.js approach leaves them as-is and relies on the browser honoring a
    /// `video.currentTime` seek to the matching live-edge position.  Safari
    /// (and some other browsers) reset `currentTime` back to 0 before content
    /// is buffered, so the player stalls forever waiting for data at position 0
    /// while content is buffered at position ~1.778 × 10⁹ s.
    ///
    /// Calling this method after session construction adjusts
    /// `SourceBuffer.timestampOffset` on every active track sink by
    /// `-offset_s`, where `offset_s` is:
    ///
    /// - `$Number$` tracks: `(initial_segment_number − start_number) × seg_dur_s`
    /// - `$Time$` (SegmentTimeline) tracks: `next_segment_time / timescale`
    ///
    /// After the call, the first live-edge segment lands at presentation
    /// time ≈ 0 in the browser.  [`Self::live_presentation_start_s`]
    /// automatically subtracts the stored offset so it also returns ≈ 0,
    /// and `bridge.seekTo(session.live_presentation_start_s())` becomes a
    /// no-op seek to 0 that every browser handles correctly.
    ///
    /// # When to call
    ///
    /// - **WASM / MSE players**: call this after [`Self::from_url_with_clock`]
    ///   and before the segment fetch loop, for live streams only.
    /// - **GStreamer players**: do NOT call this.  Use
    ///   `GstMediaSource::set_live_mode(true)` instead — it performs an
    ///   equivalent PTS subtraction via a GStreamer probe.  Double-applying
    ///   would corrupt timing.
    ///
    /// # Idempotence
    ///
    /// The offset is computed from the track state at the time of the call.
    /// Calling this a second time would re-apply the (now-zero) offset and
    /// be a no-op in practice, but it is intended to be called exactly once.
    pub fn apply_live_pts_remap(&mut self) {
        if !self.is_live() {
            return;
        }

        // Compute the offset from the primary (video, fallback audio) track.
        let offset_s = {
            let track = self.video_track.as_ref().or(self.audio_track.as_ref());
            match track {
                None => return,
                Some(t) => {
                    if t.addressing_mode == AddressingMode::Timeline {
                        t.next_segment_time
                            .map(|ticks| ticks as f64 / t.timescale as f64)
                            .unwrap_or(0.0)
                    } else {
                        let seg_dur_s = t.segment_duration_ms / 1_000.0;
                        t.initial_segment_number.saturating_sub(t.start_number) as f64
                            * seg_dur_s
                    }
                }
            }
        };

        if offset_s == 0.0 {
            tracing::debug!("apply_live_pts_remap: offset is 0 — nothing to do");
            return;
        }

        tracing::info!(
            offset_s,
            "apply_live_pts_remap: shifting timestampOffset by -{offset_s:.3} s on all sinks"
        );

        if let Some(track) = self.video_track.as_mut() {
            let current = track.sink.timestamp_offset();
            track.sink.set_timestamp_offset(current - offset_s);
        }
        if let Some(track) = self.audio_track.as_mut() {
            let current = track.sink.timestamp_offset();
            track.sink.set_timestamp_offset(current - offset_s);
        }

        self.live_pts_remap_offset_s = offset_s;
    }

    /// The minimum interval (in milliseconds) between manifest refreshes.
    pub fn minimum_update_period_ms(&self) -> Option<u64> {
        self.minimum_update_period_ms
    }

    /// Unix-epoch seconds at which the live stream ends, from
    /// `MPD@availabilityEndTime`.
    ///
    /// `None` for VOD streams or live streams without a declared end time.
    /// When present, [`Self::fetch_next_segment`] returns
    /// [`DashError::EndOfStream`] as soon as the wall clock exceeds this value,
    /// allowing cleanly scheduled broadcasts to terminate without 404 loops
    /// (DASH spec §5.3.1.2).
    pub fn availability_end_time_s(&self) -> Option<f64> {
        self.mpd.availabilityEndTime
    }

    /// Returns the next segment number that will be fetched for the given media
    /// track, or `0` when the track is absent.
    ///
    /// Useful in tests to verify that live-edge snapping produced the expected
    /// starting segment number.
    pub fn current_segment_number(&self, media_type: &MediaType) -> u64 {
        match media_type {
            MediaType::Video => self.video_track.as_ref().map(|t| t.segment_number).unwrap_or(0),
            MediaType::Audio => self.audio_track.as_ref().map(|t| t.segment_number).unwrap_or(0),
        }
    }

    /// Returns the [`RepresentationInfo`] for the currently active quality
    /// level on the given track, or `None` if the track is absent.
    ///
    /// The `bandwidth` field carries the declared bitrate in bits per second
    /// as specified by `@bandwidth` in the MPD.
    pub fn current_representation(&self, media_type: &MediaType) -> Option<&RepresentationInfo> {
        let track = match media_type {
            MediaType::Video => self.video_track.as_ref(),
            MediaType::Audio => self.audio_track.as_ref(),
        }?;
        track.representations.get(track.current_index)
    }

    /// Returns the total number of representations available for the given
    /// track, or `0` if the track is absent.
    pub fn representation_count(&self, media_type: &MediaType) -> usize {
        let track = match media_type {
            MediaType::Video => self.video_track.as_ref(),
            MediaType::Audio => self.audio_track.as_ref(),
        };
        track.map(|t| t.representations.len()).unwrap_or(0)
    }

    /// The PTS remap offset applied by [`Self::apply_live_pts_remap`], in seconds.
    ///
    /// Returns `0.0` until `apply_live_pts_remap` has been called.  After the
    /// call, every active sink's `timestampOffset` has been shifted by
    /// `-live_pts_remap_offset_s`, and `live_presentation_start_s()` subtracts
    /// this value so it returns ≈ 0 for epoch-scale live streams.
    pub fn live_pts_remap_offset_s(&self) -> f64 {
        self.live_pts_remap_offset_s
    }

    /// Returns a reference to the accumulated playback metrics for this session.
    ///
    /// The returned [`DashMetrics`] contains:
    /// - [`DashMetrics::http_requests`] — one entry per completed segment fetch
    /// - [`DashMetrics::representation_switches`] — one entry per quality switch
    /// - [`DashMetrics::buffer_level_samples`] — one buffer snapshot per fetch
    ///
    /// Each list is capped at 100 entries (oldest evicted first).
    pub fn metrics(&self) -> &DashMetrics {
        &self.dash_metrics
    }

    /// Buffered time ranges for the given media track, as reported by the
    /// underlying [`SourceBufferSink`].
    ///
    /// In the WASM / MSE player these come directly from `SourceBuffer.buffered`
    /// in the browser and reflect real decoded data.  In the stub and GStreamer
    /// implementations they reflect chunks explicitly appended with timing
    /// information.
    ///
    /// Returns an empty `Vec` when the track is absent.
    pub fn buffered(&self, media_type: MediaType) -> Vec<crate::media_source::TimeRange> {
        let track = match media_type {
            MediaType::Video => self.video_track.as_ref(),
            MediaType::Audio => self.audio_track.as_ref(),
        };
        track.map(|t| t.sink.buffered()).unwrap_or_default()
    }

    /// Returns the most recently decoded `ProducerReferenceTimeBox` (`prft`)
    /// for the given media track, or `None` if no `prft` has been seen in any
    /// fetched segment for that track.
    ///
    /// Use this to compute end-to-end latency for low-latency live streams:
    ///
    /// ```text
    /// latency_s = now_unix_s - prft.wall_clock_s
    ///             + (current_position_ticks - prft.media_time) / timescale
    /// ```
    ///
    /// `prft.media_time` is in the timescale ticks of `prft.reference_track_id`.
    /// Divide by the session's known segment timescale (derived from
    /// `SegmentTemplate@timescale`) to convert to seconds.
    pub fn last_producer_reference_time(
        &self,
        media_type: &MediaType,
    ) -> Option<&ProducerReferenceTime> {
        match media_type {
            MediaType::Video => self.video_track.as_ref()?.last_prft.as_ref(),
            MediaType::Audio => self.audio_track.as_ref()?.last_prft.as_ref(),
        }
    }

    /// Returns `true` when the session should call [`Self::refresh_manifest`]
    /// before the next segment fetch.
    ///
    /// For live streams, the DASH spec §5.4 states that when
    /// `@minimumUpdatePeriod` is absent the client may refresh as often as
    /// once per segment duration.  We therefore fall back to the video (or
    /// audio) track's segment duration when the attribute is absent, matching
    /// the dash.js `ManifestUpdater.js` behaviour.
    pub fn should_refresh_manifest(&self) -> bool {
        if !self.is_live() {
            return false;
        }
        // Resolve the effective minimum update period.
        let period_ms: f64 = match self.minimum_update_period_ms {
            Some(ms) => ms as f64,
            None => {
                // Absent → default to one segment duration.
                let seg_dur = self.video_track.as_ref()
                    .or(self.audio_track.as_ref())
                    .map(|t| t.segment_duration_ms)
                    .unwrap_or(2_000.0);
                seg_dur
            }
        };
        match self.last_manifest_refresh {
            None => true,
            Some(last_ms) => (self.clock.now_ms() - last_ms) >= period_ms,
        }
    }

    /// Refresh the live manifest if [`Self::should_refresh_manifest`] returns
    /// `true`. Called automatically by [`Self::fetch_next_segment`].
    async fn refresh_manifest_if_due(&mut self) {
        if self.should_refresh_manifest() {
            match self.refresh_manifest().await {
                Ok(()) => tracing::info!("live manifest refreshed"),
                Err(e) => tracing::warn!(
                    error = %e,
                    "manifest refresh failed; continuing with stale manifest"
                ),
            }
        }
    }

    /// Re-fetch and re-parse the manifest, updating `total_segments`,
    /// `minimumUpdatePeriod`, and `base_url` without resetting ABR or buffer state.
    ///
    /// If the primary `base_url` fails, each `<Location>` URL is tried in order
    /// (mirrors `LocationSelector.js`).  The first URL that responds successfully
    /// becomes the new `base_url` for all subsequent refreshes.  If every URL
    /// fails the last error is returned.
    pub async fn refresh_manifest(&mut self) -> Result<(), DashError> {
        tracing::info!(url = %self.base_url, "refreshing live manifest");

        let loader = ManifestLoader::new(Arc::clone(&self.fetcher));

        // Try the primary URL, then fall back through any <Location> alternatives.
        let (new_mpd, new_base_url) = {
            let primary_result = loader.load(&self.base_url).await;
            match primary_result {
                Ok(pair) => pair,
                Err(primary_err) => {
                    let mut last_err = primary_err;
                    let mut found: Option<(MPD, Url)> = None;
                    for location_url in &self.location_urls {
                        tracing::warn!(
                            primary_url = %self.base_url,
                            location_url = %location_url,
                            "manifest refresh: primary URL failed, trying Location fallback"
                        );
                        match loader.load(location_url).await {
                            Ok(pair) => {
                                found = Some(pair);
                                break;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    url = %location_url,
                                    error = %e,
                                    "manifest refresh: Location URL also failed"
                                );
                                last_err = e;
                            }
                        }
                    }
                    match found {
                        Some(pair) => pair,
                        None => return Err(last_err),
                    }
                }
            }
        };

        self.last_manifest_refresh = Some(self.clock.now_ms());
        self.base_url = new_base_url.clone();
        self.minimum_update_period_ms = new_mpd.minimumUpdatePeriod.map(|d| d.as_millis() as u64);

        // Update location_urls from the refreshed manifest.
        self.location_urls = new_mpd
            .locations
            .iter()
            .filter_map(|loc| new_base_url.join(&loc.url).ok())
            .collect();

        let presentation_duration = new_mpd.mediaPresentationDuration;
        if let Some(period) = new_mpd.periods.first() {
            if let Some(track) = self.video_track.as_mut() {
                if let Some(adaptation) = find_adaptation(period, &MediaType::Video) {
                    let (new_start, _, new_total) =
                        extract_segment_timing(adaptation, period, presentation_duration);
                    if new_total != track.total_segments {
                        tracing::info!(
                            media_type = "video",
                            old = ?track.total_segments,
                            new = ?new_total,
                            "manifest refresh: segment count updated"
                        );
                        track.total_segments = new_total;
                    }
                    // Sliding-window: live DVR window may have moved forward.
                    if new_start > track.start_number {
                        tracing::info!(
                            media_type = "video",
                            old = track.start_number,
                            new = new_start,
                            "manifest refresh: startNumber advanced (sliding window)"
                        );
                        track.start_number = new_start;
                        if track.segment_number < new_start {
                            tracing::warn!(
                                media_type = "video",
                                segment_number = track.segment_number,
                                new_start_number = new_start,
                                "segment_number behind live window — snapping to new startNumber"
                            );
                            track.segment_number = new_start;
                        }
                    }
                }
            }
            if let Some(track) = self.audio_track.as_mut() {
                if let Some(adaptation) = find_adaptation(period, &MediaType::Audio) {
                    let (new_start, _, new_total) =
                        extract_segment_timing(adaptation, period, presentation_duration);
                    if new_total != track.total_segments {
                        tracing::info!(
                            media_type = "audio",
                            old = ?track.total_segments,
                            new = ?new_total,
                            "manifest refresh: segment count updated"
                        );
                        track.total_segments = new_total;
                    }
                    // Sliding-window: live DVR window may have moved forward.
                    if new_start > track.start_number {
                        tracing::info!(
                            media_type = "audio",
                            old = track.start_number,
                            new = new_start,
                            "manifest refresh: startNumber advanced (sliding window)"
                        );
                        track.start_number = new_start;
                        if track.segment_number < new_start {
                            tracing::warn!(
                                media_type = "audio",
                                segment_number = track.segment_number,
                                new_start_number = new_start,
                                "segment_number behind live window — snapping to new startNumber"
                            );
                            track.segment_number = new_start;
                        }
                    }
                }
            }
        }

        self.mpd = new_mpd;

        // P1-1: Re-parse timeShiftBufferDepth from the refreshed manifest.
        // DVR window depth may change across refreshes (e.g. broadcaster widens
        // the window during a live event).
        if self.is_live() {
            self.time_shift_buffer_depth_ms =
                self.mpd.timeShiftBufferDepth.map(|d| d.as_millis() as f64);
        }

        // P1-2: Re-derive period_start_s for the current period.
        // In live multi-period streams the broadcaster may correct Period@start
        // in a refreshed manifest; seek_to() would use the stale value without
        // this update.
        {
            let current_idx = self.current_period_index;
            if let Some(period) = self.mpd.periods.get(current_idx) {
                let new_period_start_s = period.start.map(|d| d.as_secs_f64()).unwrap_or(0.0);
                if let Some(track) = self.video_track.as_mut() {
                    track.period_start_s = new_period_start_s;
                }
                if let Some(track) = self.audio_track.as_mut() {
                    track.period_start_s = new_period_start_s;
                }
            }
        }

        // Re-snap the live edge for both tracks.  If the segment_number has
        // fallen behind the new live edge by more than the safety margin (e.g.
        // after a long pause or network outage), advance it so the session does
        // not attempt to fetch a large backlog of expired segments.
        if self.is_live() {
            let ast_s: Option<f64> = self.mpd.availabilityStartTime;
            let spd_s: Option<f64> = self.mpd.suggestedPresentationDelay
                .map(|d| d.as_secs_f64());
            if let Some(ast) = ast_s {
                let now_s = self.clock.unix_s() + self.utc_clock_offset_s;
                for (label, track_opt) in [
                    ("video", self.video_track.as_mut()),
                    ("audio", self.audio_track.as_mut()),
                ] {
                    if let Some(track) = track_opt {
                        let new_live_edge = compute_live_edge_segment(
                            ast,
                            now_s,
                            track.segment_duration_ms / 1_000.0,
                            track.start_number,
                            spd_s,
                            track.availability_time_offset_s,
                        );
                        if track.segment_number < new_live_edge {
                            tracing::info!(
                                media_type = label,
                                old_segment = track.segment_number,
                                new_live_edge,
                                "manifest refresh: segment_number behind live edge — snapping"
                            );
                            track.segment_number = new_live_edge;
                            track.initial_segment_number = new_live_edge;
                        }
                    }
                }
            }
        }

        // For $Time$-addressed (Timeline) tracks, validate next_segment_time
        // against the refreshed SegmentTimeline.  If the referenced time is no
        // longer present (it has slid out of the live window), reset it to None
        // so the next fetch starts from the earliest available entry rather than
        // signalling a spurious EndOfStream.
        if let Some(period) = self.mpd.periods.first() {
            for (media_type_ref, track_opt) in [
                (MediaType::Video, self.video_track.as_mut()),
                (MediaType::Audio, self.audio_track.as_mut()),
            ] {
                if let Some(track) = track_opt {
                    if track.addressing_mode != AddressingMode::Timeline {
                        continue;
                    }
                    if let Some(nst) = track.next_segment_time {
                        if let Some(adaptation) = find_adaptation(period, &media_type_ref) {
                            let repr_id = track
                                .representations
                                .get(track.current_index)
                                .map(|ri| ri.id.as_str())
                                .unwrap_or("");
                            let repr_obj = adaptation
                                .representations
                                .iter()
                                .find(|r| r.id.as_deref() == Some(repr_id));
                            if let Some(tmpl) =
                                merge_segment_templates(repr_obj, adaptation, period)
                            {
                                if let Some(tl) = &tmpl.SegmentTimeline {
                                    if find_timeline_segment(&tl.segments, Some(nst)).is_none()
                                    {
                                        tracing::warn!(
                                            media_type = %media_type_ref,
                                            next_segment_time = nst,
                                            "manifest refresh: next_segment_time expired \
                                             from new timeline — resetting to oldest available"
                                        );
                                        track.next_segment_time = None;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        tracing::info!(
            minimum_update_period_ms = ?self.minimum_update_period_ms,
            "manifest refreshed successfully"
        );
        Ok(())
    }

    // ── ABR ───────────────────────────────────────────────────────────────────

    /// Run ABR for the given media type without changing any state.
    pub fn select_quality(
        &self,
        media_type: &MediaType,
        metrics: &AbrMetrics,
    ) -> Result<QualityDecision, DashError> {
        let track = self.track(media_type)?;
        Ok(self.abr.select(&track.representations, metrics))
    }

    /// Re-run ABR with the supplied metrics and apply the decision immediately.
    pub fn update_metrics(&mut self, media_type: &MediaType, metrics: AbrMetrics) {
        let reps = match self.track(media_type) {
            Ok(t) => t.representations.clone(),
            Err(_) => return,
        };

        let decision = self.abr.select(&reps, &metrics);

        if let Ok(track) = self.track_mut(media_type) {
            if decision.index != track.current_index {
                tracing::info!(
                    media_type = %media_type,
                    old_index = track.current_index,
                    new_index = decision.index,
                    reason = ?decision.reason,
                    "quality switch"
                );
                track.current_index = decision.index;
            }
        }
        self.abr.record_switch();
    }

    // ── Fetching ──────────────────────────────────────────────────────────────

    /// Fetch the next segment for the given media type using the current ABR
    /// quality decision, then append it to the corresponding source buffer.
    ///
    /// At MPD period boundaries, the session automatically advances to the next
    /// period and reinitialises the track without returning an error.
    ///
    /// For **live streams**, the manifest is refreshed automatically at the
    /// start of each call when `MPD@minimumUpdatePeriod` has elapsed.
    ///
    /// When a live segment returns HTTP 404 (not yet generated), the call waits
    /// one segment duration and retries once before propagating the error.
    ///
    /// Returns [`DashError::EndOfStream`] when all segments in all periods have
    /// been fetched.
    pub async fn fetch_next_segment(
        &mut self,
        media_type: MediaType,
        metrics: &AbrMetrics,
    ) -> Result<QualityDecision, DashError> {
        loop {
            match self.fetch_next_segment_inner(media_type.clone(), metrics).await {
                Err(DashError::EndOfStream { .. }) => {
                    if self.try_advance_period().await? {
                        continue; // retry with new period
                    }
                    return Err(DashError::EndOfStream {
                        media_type: media_type.to_string(),
                    });
                }
                // Live segment not yet generated: wait one segment duration then
                // retry once.  If still unavailable, propagate as AllBaseUrlsFailed.
                Err(DashError::SegmentNotYetAvailable { .. }) => {
                    let seg_dur_ms = self
                        .track(&media_type)
                        .map(|t| t.segment_duration_ms)
                        .unwrap_or(2_000.0);
                    tracing::info!(
                        media_type = %media_type,
                        wait_ms = seg_dur_ms as u64,
                        "live segment not yet available — waiting one segment duration before retry"
                    );
                    Delay::new(core::time::Duration::from_millis(seg_dur_ms as u64)).await;
                    // Single retry: if still 404, propagate the underlying error.
                    return self.fetch_next_segment_inner(media_type.clone(), metrics).await;
                }
                other => return other,
            }
        }
    }

    /// Inner implementation of [`Self::fetch_next_segment`] for a single period.
    ///
    /// Returns [`DashError::EndOfStream`] at period boundaries; the public
    /// wrapper catches this and advances the period before retrying.
    async fn fetch_next_segment_inner(
        &mut self,
        media_type: MediaType,
        metrics: &AbrMetrics,
    ) -> Result<QualityDecision, DashError> {
        self.refresh_manifest_if_due().await;

        // Availability-end-time check (DASH spec §5.3.1.2).
        //
        // When MPD@availabilityEndTime is set the live broadcast has a declared
        // end time. Once the wall clock passes it, no new segments will be
        // generated; return EndOfStream cleanly rather than looping on 404s.
        if self.is_live() {
            if let Some(aet_s) = self.mpd.availabilityEndTime {
                let now_s = self.clock.unix_s() + self.utc_clock_offset_s;
                if now_s > aet_s {
                    tracing::info!(
                        media_type = %media_type,
                        availability_end_time_s = aet_s,
                        now_s,
                        "availabilityEndTime passed — signalling EndOfStream"
                    );
                    return Err(DashError::EndOfStream {
                        media_type: media_type.to_string(),
                    });
                }
            }
        }

        // Segment-count EOS check.
        {
            let track = self.track(&media_type)?;
            if let Some(total) = track.total_segments {
                let fetched = track.segment_number.saturating_sub(track.start_number);
                if fetched >= total {
                    tracing::info!(
                        media_type = %media_type,
                        total_segments = total,
                        "all segments fetched — signalling EndOfStream"
                    );
                    return Err(DashError::EndOfStream {
                        media_type: media_type.to_string(),
                    });
                }
            }
        }

        // Time-based EOS check.
        {
            let (total_segments, seg_dur_ms) = {
                let track = self.track(&media_type)?;
                (track.total_segments, track.segment_duration_ms)
            };
            if let Some(total) = total_segments {
                const EOS_EPSILON: f64 = 0.00001;
                let presentation_end_s = total as f64 * seg_dur_ms / 1000.0;
                let current_time_s = self.estimated_playback_position_s();
                let time_to_stream_end_s = (presentation_end_s - current_time_s).max(0.0);
                let buffer_level_s = self.estimated_buffer_level_ms(&media_type) / 1000.0;
                if time_to_stream_end_s - buffer_level_s < EOS_EPSILON {
                    tracing::info!(
                        media_type = %media_type,
                        time_to_stream_end_s,
                        buffer_level_s,
                        "buffer covers end of stream — signalling EndOfStream"
                    );
                    return Err(DashError::EndOfStream {
                        media_type: media_type.to_string(),
                    });
                }
            }
        }

        // DVR window floor check (live streams only).
        //
        // If `MPD@timeShiftBufferDepth` is set, segments older than
        // `live_edge − floor(tsbd / seg_dur)` are outside the DVR window and
        // their CDN content may have been deleted.  Advance `segment_number`
        // past the floor so the session does not repeatedly attempt expired
        // segments (DASH spec §4.5.3).
        if self.is_live() {
            if let (Some(tsbd_ms), Some(ast)) = (
                self.time_shift_buffer_depth_ms,
                self.mpd.availabilityStartTime,
            ) {
                let track = self.track(&media_type)?;
                let seg_dur_s = track.segment_duration_ms / 1_000.0;
                let now_s = self.clock.unix_s() + self.utc_clock_offset_s;
                let live_edge = compute_live_edge_segment(
                    ast,
                    now_s,
                    seg_dur_s,
                    track.start_number,
                    self.mpd.suggestedPresentationDelay.map(|d| d.as_secs_f64()),
                    track.availability_time_offset_s,
                );
                let window_segments = (tsbd_ms / 1_000.0 / seg_dur_s).floor() as u64;
                let dvr_floor = live_edge.saturating_sub(window_segments).max(track.start_number);
                if track.segment_number < dvr_floor {
                    tracing::warn!(
                        media_type = %media_type,
                        segment_number = track.segment_number,
                        dvr_floor,
                        live_edge,
                        "segment outside DVR window — advancing to floor"
                    );
                    let track = self.track_mut(&media_type)?;
                    track.segment_number = dvr_floor;
                }
            }
        }

        // Run ABR.
        let decision = {
            let track = self.track(&media_type)?;
            let uncapped = self.abr.select(&track.representations, metrics);
            // Apply bitrate cap: clamp to the highest representation whose
            // bandwidth is at or below the cap.  Always allow index 0 even
            // when it exceeds the cap (avoids a situation where no quality is
            // selectable).
            let max_bps = match &media_type {
                MediaType::Video => self.video_max_bitrate_bps,
                MediaType::Audio => self.audio_max_bitrate_bps,
            };
            if let Some(cap) = max_bps {
                let capped_idx = track
                    .representations
                    .iter()
                    .rposition(|r| r.bandwidth <= cap)
                    .unwrap_or(0);
                if capped_idx < uncapped.index {
                    tracing::debug!(
                        media_type = %media_type,
                        uncapped_index = uncapped.index,
                        capped_index = capped_idx,
                        cap_bps = cap,
                        "bitrate cap applied"
                    );
                    QualityDecision { index: capped_idx, reason: uncapped.reason }
                } else {
                    uncapped
                }
            } else {
                uncapped
            }
        };

        // Apply quality switch if needed.
        // Returns Some(from_index) on a switch, None when quality is unchanged.
        let switched_from: Option<usize> = {
            let track = self.track_mut(&media_type)?;
            if decision.index != track.current_index {
                let from_index = track.current_index;
                tracing::info!(
                    media_type = %media_type,
                    old = from_index,
                    new = decision.index,
                    reason = ?decision.reason,
                    "quality switch"
                );
                track.current_index = decision.index;
                self.abr.record_switch();
                Some(from_index)
            } else {
                None
            }
        };
        let quality_switched = switched_from.is_some();

        // Record a representation-switch metric when a quality change occurred.
        if let Some(from_index) = switched_from {
            let wall_ms = (self.clock.unix_s() + self.utc_clock_offset_s) * 1_000.0;
            self.dash_metrics.push_switch(RepresentationSwitchRecord {
                wall_ms,
                media_type: media_type.clone(),
                from_index,
                to_index: decision.index,
                reason: decision.reason.clone(),
            });
        }

        // On a quality switch, re-append the init segment so the decoder
        // sees the correct codec configuration.  Use the cache (populated by
        // `fetch_init_segment`) to avoid a network round-trip; fall back to a
        // live fetch (which also populates the cache) on a cache miss.
        if quality_switched {
            let (new_rep_id, mt_str) = {
                let track = self.track(&media_type)?;
                (
                    track.representations[track.current_index].id.clone(),
                    media_type.to_string(),
                )
            };
            let cache_key = format!("{}/{}", mt_str, new_rep_id);
            let cached = self.init_cache.get(&cache_key).cloned();
            if let Some(data) = cached {
                tracing::debug!(
                    media_type = %media_type,
                    rep_id = %new_rep_id,
                    "quality switch: re-appending init segment from cache"
                );
                let chunk = Chunk::new(data, None, None, format!("{}/init", media_type));
                let track = self.track_mut(&media_type)?;
                track.sink.append(chunk).await?;
            } else {
                tracing::debug!(
                    media_type = %media_type,
                    rep_id = %new_rep_id,
                    "quality switch: init segment not cached, fetching"
                );
                self.fetch_init_segment(media_type.clone()).await?;
            }
        }

        // ── First-play init segment ───────────────────────────────────────────
        // If no quality switch occurred (the common case on the very first call),
        // the block above did not run.  Ensure the init segment has been sent
        // before any media data reaches the source buffer.
        if !quality_switched && !self.track(&media_type)?.init_segment_sent {
            self.fetch_init_segment(media_type.clone()).await?;
        }

        // ── SegmentBase: lazy sidx population ────────────────────────────────
        // For SegmentBase tracks, the sidx box must be fetched once to learn
        // the byte ranges for each subsegment.  After this call,
        // `track.segment_byte_ranges` is guaranteed to be `Some(_)` and
        // `track.total_segments` is set so that the early EOS checks fire
        // correctly on subsequent calls.
        {
            let is_base = matches!(
                self.track(&media_type)?.addressing_mode,
                AddressingMode::Base { .. }
            );
            if is_base && self.track(&media_type)?.segment_byte_ranges.is_none() {
                self.fetch_base_sidx(&media_type).await?;
            }
        }

        // For SegmentBase tracks, derive the byte range for the current
        // subsegment from the now-populated index.
        let base_byte_range: Option<(u64, u64)> = {
            let track = self.track(&media_type)?;
            if let AddressingMode::Base { .. } = &track.addressing_mode {
                let seg_idx = track.segment_number as usize;
                let ranges = track
                    .segment_byte_ranges
                    .as_ref()
                    .expect("populated by fetch_base_sidx above");
                if seg_idx >= ranges.len() {
                    tracing::info!(
                        media_type = %media_type,
                        seg_idx,
                        total = ranges.len(),
                        "SegmentBase index exhausted — signalling EndOfStream"
                    );
                    return Err(DashError::EndOfStream {
                        media_type: media_type.to_string(),
                    });
                }
                let e = &ranges[seg_idx];
                Some((e.byte_offset, e.byte_offset + e.byte_length as u64 - 1))
            } else {
                None
            }
        };

        // Build the expanded segment URL.
        let (candidates, expanded_path, seg_number, maybe_timeline_info) = {
            let track = self.track(&media_type)?;
            let rep = &track.representations[track.current_index];
            let seg_num = track.segment_number;
            let addressing_mode = track.addressing_mode.clone();
            let next_seg_time = track.next_segment_time;

            let period = &self.mpd.periods[self.current_period_index];
            let adaptation = find_adaptation(period, &media_type).ok_or_else(|| {
                DashError::NoRepresentations {
                    media_type: media_type.to_string(),
                }
            })?;

            let repr_obj: Option<&Representation> = adaptation
                .representations
                .iter()
                .find(|r| r.id.as_deref() == Some(rep.id.as_str()));

            let repr_base_urls = repr_obj.map(|r| r.BaseURL.as_slice()).unwrap_or(&[]);

            if let AddressingMode::Base { ref media_url, .. } = addressing_mode {
                // ─ SegmentBase addressing ────────────────────────────────────
                // The media file is the effective base URL (or a relative
                // path within it).  Byte-range is handled separately via
                // `base_byte_range` above.
                let candidates = collect_candidate_base_urls(
                    &self.base_url,
                    &self.mpd.base_url,
                    period,
                    adaptation,
                    repr_base_urls,
                );
                (candidates, media_url.clone(), seg_num, None)
            } else if addressing_mode == AddressingMode::List {
                // ─ SegmentList addressing ────────────────────────────────────
                let sl = merge_segment_lists(repr_obj, adaptation, period)
                    .ok_or(DashError::SegmentUrlUnresolvable)?;

                // `seg_num − start_number` gives the 0-based index into segment_urls.
                // (start_number defaults to 0 so this is backwards-compatible.)
                let seg_idx = seg_num.saturating_sub(track.start_number) as usize;
                let seg_url_entry =
                    sl.segment_urls.get(seg_idx).ok_or_else(|| {
                        tracing::info!(
                            media_type = %media_type,
                            seg_idx,
                            "SegmentList exhausted — signalling EndOfStream"
                        );
                        DashError::EndOfStream {
                            media_type: media_type.to_string(),
                        }
                    })?;

                // `mediaRange` (byte-range SegmentList) is not yet supported.
                if seg_url_entry.mediaRange.is_some() {
                    tracing::warn!(
                        media_type = %media_type,
                        seg_idx,
                        "SegmentList entry uses mediaRange (byte-range); not yet supported"
                    );
                    return Err(DashError::SegmentUrlUnresolvable);
                }

                let media_url_str = seg_url_entry
                    .media
                    .as_deref()
                    .ok_or(DashError::SegmentUrlUnresolvable)?;

                let candidates = collect_candidate_base_urls(
                    &self.base_url,
                    &self.mpd.base_url,
                    period,
                    adaptation,
                    repr_base_urls,
                );
                (candidates, media_url_str.to_owned(), seg_num, None)
            } else {
            // ─ SegmentTemplate addressing (Number or Timeline) ───────────────
            let merged = merge_segment_templates(repr_obj, adaptation, period)
                .ok_or(DashError::SegmentUrlUnresolvable)?;

            let media_tmpl = merged
                .media
                .as_deref()
                .ok_or(DashError::SegmentUrlUnresolvable)?;

            let (path, timeline_info) = if addressing_mode == AddressingMode::Timeline {
                let tl = merged
                    .SegmentTimeline
                    .as_ref()
                    .ok_or(DashError::SegmentUrlUnresolvable)?;
                let (seg_time, seg_dur) =
                    find_timeline_segment(&tl.segments, next_seg_time).ok_or_else(|| {
                        tracing::info!(
                            media_type = %media_type,
                            next_time = ?next_seg_time,
                            "SegmentTimeline exhausted — signalling EndOfStream"
                        );
                        DashError::EndOfStream {
                            media_type: media_type.to_string(),
                        }
                    })?;
                let p = expand_template(media_tmpl, &rep.id, rep.bandwidth, seg_num, Some(seg_time));
                (p, Some((seg_time, seg_dur)))
            } else {
                let p = expand_template(media_tmpl, &rep.id, rep.bandwidth, seg_num, None);
                (p, None)
            };

            let candidates = collect_candidate_base_urls(
                &self.base_url,
                &self.mpd.base_url,
                period,
                adaptation,
                repr_base_urls,
            );
            (candidates, path, seg_num, timeline_info)
            } // end SegmentTemplate branch
        };

        // Fetch — try each CDN candidate in priority order, skipping any that
        // are currently blacklisted.  If every candidate is blacklisted, the
        // exclusion is overridden and all candidates are retried (safety net to
        // prevent the session from permanently stalling).
        let fetcher = Arc::clone(&self.fetcher);
        let abandon_ms = self.abandon_timeout_ms;
        let mt_str = media_type.to_string();

        // Remove expired entries opportunistically so the map stays bounded.
        self.cdn_blacklist.evict_expired();

        // Build the effective candidate list, filtering out blacklisted origins.
        // Fall back to the full list when everything is blacklisted.
        let effective: Vec<usize> = {
            let non_bl: Vec<usize> = (0..candidates.len())
                .filter(|&i| !self.cdn_blacklist.is_blacklisted(&candidates[i].service_location))
                .collect();
            if non_bl.is_empty() {
                tracing::warn!(
                    media_type = %media_type,
                    "all CDN candidates are currently blacklisted — retrying all"
                );
                (0..candidates.len()).collect()
            } else {
                non_bl
            }
        };

        let mut last_err: DashError = DashError::AllBaseUrlsFailed {
            last_error: "no candidates".into(),
        };
        let mut opt_response: Option<FetchResponse> = None;

        'candidates: for &idx in &effective {
            let candidate = &candidates[idx];
            let segment_url = candidate
                .url
                .join(&expanded_path)
                .map_err(|_| DashError::SegmentUrlUnresolvable)?;

            tracing::debug!(
                media_type = %media_type,
                segment_number = seg_number,
                url = %segment_url,
                service_location = %candidate.service_location,
                "fetching segment"
            );

            let request = {
                let req = FetchRequest::new(segment_url.clone(), RequestType::MediaSegment);
                if let Some((br_start, br_end)) = base_byte_range {
                    req.with_byte_range(br_start, br_end)
                } else {
                    req
                }
            };

            let fetch_result: Result<FetchResponse, DashError> = if let Some(ms) = abandon_ms {
                let delay = Delay::new(core::time::Duration::from_millis(ms));
                match fut::select(
                    Box::pin(fetcher.fetch(request)),
                    Box::pin(delay),
                )
                .await
                {
                    Either::Left((Ok(resp), _)) => Ok(resp),
                    Either::Left((Err(e), _)) => Err(e.into()),
                    Either::Right(_) => {
                        return Err(DashError::SegmentAbandoned {
                            media_type: mt_str.clone(),
                        });
                    }
                }
            } else {
                fetcher.fetch(request).await.map_err(Into::into)
            };

            match fetch_result {
                Ok(resp) => {
                    opt_response = Some(resp);
                    break 'candidates;
                }
                Err(e) => {
                    // HTTP 404 on a live stream means the segment has not been
                    // generated yet (we are too close to the live edge).  Do
                    // not blacklist the CDN — the failure is transient.  Signal
                    // SegmentNotYetAvailable so fetch_next_segment can wait and
                    // retry (mirrors FragmentLoader.js behaviour in dash.js).
                    let is_live_404 = self.is_live()
                        && matches!(
                            e,
                            DashError::ManifestFetch(
                                crate::fetch::FetchError::Http { status: 404, .. }
                            )
                        );
                    if is_live_404 {
                        tracing::debug!(
                            url = %segment_url,
                            segment_number = seg_number,
                            "live segment not yet available (HTTP 404) — will wait and retry"
                        );
                        last_err = DashError::SegmentNotYetAvailable {
                            media_type: mt_str.clone(),
                        };
                    } else {
                        tracing::warn!(
                            url = %segment_url,
                            service_location = %candidate.service_location,
                            error = %e,
                            "segment fetch failed — blacklisting CDN and trying next base URL"
                        );
                        self.cdn_blacklist.record_failure(&candidate.service_location);
                        last_err = DashError::AllBaseUrlsFailed {
                            last_error: e.to_string(),
                        };
                    }
                }
            }
        }

        let response = opt_response.ok_or(last_err)?;

        // Update per-track throughput tracker and transfer latency from this response.
        {
            let track = self.track_mut(&media_type)?;
            track.last_transfer_ms = response.transfer_duration_ms as f64;
            if let Some(bps) = response.throughput_bps() {
                let elapsed_s = response.transfer_duration_ms as f64 / 1_000.0;
                track.throughput_tracker.record(bps, elapsed_s);
            }
            tracing::debug!(
                media_type = %media_type,
                segment_number = seg_number,
                throughput_bps = track.throughput_tracker.estimate(),
                transfer_ms = track.last_transfer_ms,
                "throughput updated"
            );
        }

        // Record HTTP request metrics and a buffer-level sample.
        {
            let wall_ms = (self.clock.unix_s() + self.utc_clock_offset_s) * 1_000.0;
            let (quality_index, buffer_level_ms) = {
                let track = self.track(&media_type)?;
                let position_s = self.estimated_playback_position_s();
                let level = track
                    .sink
                    .buffered()
                    .last()
                    .map(|r| (r.end - position_s).max(0.0) * 1_000.0)
                    .unwrap_or(0.0);
                (track.current_index, level)
            };
            self.dash_metrics.push_http(HttpRequestRecord {
                wall_ms,
                media_type: media_type.clone(),
                segment_number: seg_number,
                url: response.final_url.to_string(),
                bytes_received: response.bytes_received,
                transfer_duration_ms: response.transfer_duration_ms as f64,
                throughput_bps: response.throughput_bps(),
                quality_index,
            });
            self.dash_metrics.push_buffer_level(BufferLevelSample {
                wall_ms,
                media_type: media_type.clone(),
                level_ms: buffer_level_ms,
            });
        }

        let sidx = parse_sidx(&response.data);
        if sidx.is_some() {
            tracing::debug!(
                media_type = %media_type,
                segment_number = seg_number,
                start_s = sidx.map(|s| s.start_s).unwrap_or(0.0),
                end_s   = sidx.map(|s| s.end_s).unwrap_or(0.0),
                "sidx timing extracted"
            );
        }

        // Parse ProducerReferenceTime (prft) box for latency measurement.
        if let Some(prft) = parse_prft(&response.data) {
            tracing::debug!(
                media_type = %media_type,
                segment_number = seg_number,
                wall_clock_s = prft.wall_clock_s,
                media_time = prft.media_time,
                reference_track_id = prft.reference_track_id,
                "prft: producer reference time extracted"
            );
            let track = self.track_mut(&media_type)?;
            track.last_prft = Some(prft);
        }

        // Parse and broadcast any in-band DASH events embedded in this segment.
        let emsg_events = parse_all_emsg(&response.data);
        if !emsg_events.is_empty() {
            tracing::debug!(
                media_type = %media_type,
                segment_number = seg_number,
                count = emsg_events.len(),
                "emsg: dispatching in-band events"
            );
            for emsg in emsg_events {
                tracing::debug!(
                    scheme_id_uri = %emsg.scheme_id_uri,
                    value = %emsg.value,
                    presentation_time_s = emsg.presentation_time_s,
                    id = emsg.id,
                    "emsg event"
                );
                let event = DashEvent {
                    media_type: media_type.clone(),
                    scheme_id_uri: emsg.scheme_id_uri,
                    value: emsg.value,
                    presentation_time_s: emsg.presentation_time_s,
                    duration_s: emsg.duration_s,
                    id: emsg.id,
                    message_data: emsg.message_data,
                };
                // Errors here mean no subscribers or channel full — both are fine.
                let _ = self.event_tx.try_broadcast(event);
            }
        }

        let chunk = Chunk::new(
            response.data,
            sidx.map(|s| s.start_s),
            sidx.map(|s| s.end_s),
            format!("{}/{}", media_type, seg_number),
        );

        {
            let track = self.track_mut(&media_type)?;
            track.sink.append(chunk).await?;
            track.segment_number += 1;
            if let Some((seg_time, seg_dur)) = maybe_timeline_info {
                track.next_segment_time = Some(seg_time + seg_dur);
            }
        }

        Ok(decision)
    }

    // ── Period management ─────────────────────────────────────────────────────

    /// Attempt to advance to the next MPD `Period`.
    ///
    /// When `Ok(true)` is returned, `current_period_index` has been incremented,
    /// tracks have been reinitialised, and init segments for the new period have
    /// been fetched.  The caller should retry the segment fetch.
    ///
    /// When `Ok(false)` is returned, there are no more periods and the caller
    /// should propagate [`DashError::EndOfStream`].
    async fn try_advance_period(&mut self) -> Result<bool, DashError> {
        let next_idx = self.current_period_index + 1;
        if next_idx >= self.mpd.periods.len() {
            return Ok(false);
        }

        tracing::info!(
            from = self.current_period_index,
            to = next_idx,
            "period boundary: advancing to next MPD period"
        );
        self.current_period_index = next_idx;

        // Clear init-segment cache: the new period may have different codec
        // configurations even for the same representation IDs.
        self.init_cache.clear();

        let presentation_duration = self.mpd.mediaPresentationDuration;

        // Read all period data into owned structs so that the immutable
        // borrow on `self.mpd` is released before we mutably borrow tracks.
        let (period_start_s, effective_duration_s, video_reinit, audio_reinit) = {
            let period = &self.mpd.periods[next_idx];
            let period_start_s = period.start.map(|d| d.as_secs_f64()).unwrap_or(0.0);
            let eff = period.duration.or(presentation_duration);
            let effective_duration_s = eff.map(|d| d.as_secs_f64());
            let video_reinit = find_adaptation(period, &MediaType::Video)
                .map(|a| compute_track_reinit(a, period, presentation_duration));
            let audio_reinit = find_adaptation(period, &MediaType::Audio)
                .map(|a| compute_track_reinit(a, period, presentation_duration));
            (period_start_s, effective_duration_s, video_reinit, audio_reinit)
        };
        // `self.mpd` immutable borrow ends here.

        let ms_duration = presentation_duration.map(|d| d.as_secs_f64());
        let stream_info = StreamInfo {
            start: period_start_s,
            duration: effective_duration_s,
        };

        if let (Some(ri), Some(track)) = (&video_reinit, self.video_track.as_mut()) {
            apply_track_reinit(track, ri, &stream_info, ms_duration).await?;
        }
        if let (Some(ri), Some(track)) = (&audio_reinit, self.audio_track.as_mut()) {
            apply_track_reinit(track, ri, &stream_info, ms_duration).await?;
        }

        // Fetch init segments for the new period's tracks.
        // Bracket with the gap-controller guard so that gap-jump detection is
        // suppressed while the source buffer is being replaced — mirroring the
        // BUFFER_REPLACEMENT_STARTED / TRACK_CHANGE_RENDERED event pair in
        // dash.js's GapController.js.
        self.gap_controller.on_track_switch_started();
        if self.video_track.is_some() {
            self.fetch_init_segment(MediaType::Video).await?;
        }
        if self.audio_track.is_some() {
            self.fetch_init_segment(MediaType::Audio).await?;
        }
        self.gap_controller.on_track_switch_ended();

        Ok(true)
    }

    /// Fetch and parse the `sidx` index for a `SegmentBase` track.
    ///
    /// Issues a byte-range `GET` for the `indexRange` declared in the MPD,
    /// parses the `sidx` box with [`parse_sidx_entries`], and stores the
    /// resulting subsegment table in [`TrackState::segment_byte_ranges`].
    /// Also updates [`TrackState::total_segments`] so that the normal EOS
    /// segment-count check fires correctly after the index is known.
    ///
    /// No-op when `segment_byte_ranges` is already populated.
    async fn fetch_base_sidx(&mut self, media_type: &MediaType) -> Result<(), DashError> {
        let (media_url_str, index_range) = {
            let track = self.track(media_type)?;
            if track.segment_byte_ranges.is_some() {
                return Ok(()); // already populated
            }
            match &track.addressing_mode {
                AddressingMode::Base { media_url, index_range, .. } => {
                    (media_url.clone(), *index_range)
                }
                _ => return Ok(()), // not a Base track
            }
        };

        let period = &self.mpd.periods[self.current_period_index];
        let adaptation = find_adaptation(period, media_type).ok_or_else(|| {
            DashError::NoRepresentations {
                media_type: media_type.to_string(),
            }
        })?;        let first_repr = adaptation.representations.first();
        let repr_base_urls = first_repr.map(|r| r.BaseURL.as_slice()).unwrap_or(&[]);

        let sidx_base = resolve_effective_base_url(
            &self.base_url,
            &self.mpd.base_url,
            period,
            adaptation,
            repr_base_urls,
        );
        let sidx_url = sidx_base
            .join(&media_url_str)
            .map_err(|_| DashError::SegmentUrlUnresolvable)?;

        tracing::debug!(
            media_type = %media_type,
            url = %sidx_url,
            range_start = index_range.0,
            range_end = index_range.1,
            "SegmentBase: fetching sidx index"
        );

        let request = FetchRequest::new(sidx_url, RequestType::InitializationSegment)
            .with_byte_range(index_range.0, index_range.1);
        let response = self.fetcher.fetch(request).await?;

        // `index_end_offset` = first byte after the fetched range.
        let index_end_offset = index_range.1 + 1;
        let entries = parse_sidx_entries(&response.data, index_end_offset)
            .ok_or(DashError::SegmentUrlUnresolvable)?;

        let n = entries.len() as u64;
        tracing::info!(
            media_type = %media_type,
            total_segments = n,
            "SegmentBase: sidx parsed, subsegment table populated"
        );

        let track = self.track_mut(media_type)?;
        track.total_segments = Some(n);
        track.segment_byte_ranges = Some(entries);
        Ok(())
    }

    /// Fetch and append the initialisation segment for the given media type.
    ///
    /// The fetched bytes are stored in the internal init-segment cache
    /// (`init_cache`).  Subsequent quality switches inside
    /// [`Self::fetch_next_segment`] will re-append from the cache without
    /// touching the network.
    ///
    /// Uses the same CDN blacklist-aware candidate loop as
    /// [`Self::fetch_next_segment`] so that CDN fallback and blacklisting work
    /// identically for init segments.
    pub async fn fetch_init_segment(&mut self, media_type: MediaType) -> Result<usize, DashError> {
        let (init_path, candidates, byte_range, rep_id) = {
            let track = self.track(&media_type)?;
            let rep = &track.representations[track.current_index];
            let (path, cands, br) = self.resolve_init_path_and_candidates(&media_type, rep)?;
            (path, cands, br, rep.id.clone())
        };

        tracing::debug!(
            media_type = %media_type,
            init_path = %init_path,
            byte_range = ?byte_range,
            "fetching init segment"
        );

        // CDN blacklist-aware candidate loop — mirrors the media segment loop.
        self.cdn_blacklist.evict_expired();
        let effective: Vec<usize> = {
            let non_bl: Vec<usize> = (0..candidates.len())
                .filter(|&i| !self.cdn_blacklist.is_blacklisted(&candidates[i].service_location))
                .collect();
            if non_bl.is_empty() {
                tracing::warn!(
                    media_type = %media_type,
                    "all CDN candidates blacklisted for init segment — retrying all"
                );
                (0..candidates.len()).collect()
            } else {
                non_bl
            }
        };

        let fetcher = Arc::clone(&self.fetcher);
        let mut last_err = DashError::AllBaseUrlsFailed {
            last_error: "no candidates".into(),
        };
        let mut opt_response: Option<FetchResponse> = None;

        for &idx in &effective {
            let candidate = &candidates[idx];
            let init_url = candidate
                .url
                .join(&init_path)
                .map_err(|_| DashError::SegmentUrlUnresolvable)?;

            tracing::debug!(
                media_type = %media_type,
                url = %init_url,
                service_location = %candidate.service_location,
                "fetching init segment from candidate"
            );

            let req = FetchRequest::new(init_url.clone(), RequestType::InitializationSegment);
            let req = if let Some((start, end)) = byte_range {
                req.with_byte_range(start, end)
            } else {
                req
            };

            match fetcher.fetch(req).await {
                Ok(resp) => {
                    opt_response = Some(resp);
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        url = %init_url,
                        service_location = %candidate.service_location,
                        error = %e,
                        "init segment fetch failed — blacklisting CDN and trying next"
                    );
                    self.cdn_blacklist.record_failure(&candidate.service_location);
                    last_err = DashError::AllBaseUrlsFailed {
                        last_error: e.to_string(),
                    };
                }
            }
        }

        let response = opt_response.ok_or(last_err)?;
        let bytes_received = response.data.len();

        // Populate the cache so quality-switch re-appends are free.
        let cache_key = format!("{}/{}", media_type, rep_id);
        self.init_cache.insert(cache_key, response.data.clone());

        let chunk = Chunk::new(response.data, None, None, format!("{}/init", media_type));

        {
            let track = self.track_mut(&media_type)?;
            track.sink.append(chunk).await?;
            track.init_segment_sent = true;
        }

        tracing::info!(
            media_type = %media_type,
            bytes = bytes_received,
            "init segment appended"
        );

        Ok(bytes_received)
    }

    /// Signal that no more segments will be appended.
    pub async fn end_of_stream(&mut self) -> Result<(), DashError> {
        tracing::info!("signalling end of stream");
        self.media_source.end_of_stream().await
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn track(&self, media_type: &MediaType) -> Result<&TrackState, DashError> {
        let opt = match media_type {
            MediaType::Video => self.video_track.as_ref(),
            MediaType::Audio => self.audio_track.as_ref(),
        };
        opt.ok_or_else(|| DashError::NoRepresentations {
            media_type: media_type.to_string(),
        })
    }

    fn track_mut(&mut self, media_type: &MediaType) -> Result<&mut TrackState, DashError> {
        let opt = match media_type {
            MediaType::Video => self.video_track.as_mut(),
            MediaType::Audio => self.audio_track.as_mut(),
        };
        opt.ok_or_else(|| DashError::NoRepresentations {
            media_type: media_type.to_string(),
        })
    }

    /// Resolve the initialisation segment as a `(path, candidates, byte_range)` triple.
    ///
    /// - `path` — URL path relative to each CDN candidate (the template-expanded
    ///   init path or `sourceURL`).
    /// - `candidates` — ordered CDN candidates from [`collect_candidate_base_urls`].
    /// - `byte_range` — set for `SegmentBase.Initialization@range` fetches.
    ///
    /// Used by [`Self::fetch_init_segment`] to implement CDN-aware fallback for
    /// init segment fetches (same logic as the media segment candidate loop).
    #[allow(clippy::type_complexity)]
    fn resolve_init_path_and_candidates(
        &self,
        media_type: &MediaType,
        rep: &RepresentationInfo,
    ) -> Result<(String, Vec<CandidateUrl>, Option<(u64, u64)>), DashError> {
        let period = &self.mpd.periods[self.current_period_index];
        let adaptation =
            find_adaptation(period, media_type).ok_or_else(|| DashError::NoRepresentations {
                media_type: media_type.to_string(),
            })?;

        let repr_obj: Option<&Representation> = adaptation
            .representations
            .iter()
            .find(|r| r.id.as_deref() == Some(rep.id.as_str()));

        let repr_base_urls = repr_obj.map(|r| r.BaseURL.as_slice()).unwrap_or(&[]);
        let candidates = collect_candidate_base_urls(
            &self.base_url,
            &self.mpd.base_url,
            period,
            adaptation,
            repr_base_urls,
        );

        // ─ SegmentTemplate init ──────────────────────────────────────────────
        // DASH spec §5.3.9.5: the <Initialization> child element takes
        // precedence over the @initialization attribute when both are present.
        if let Some(tmpl) = merge_segment_templates(repr_obj, adaptation, period) {
            let init_tmpl = tmpl
                .Initialization.as_ref().and_then(|i| i.sourceURL.as_deref())
                .or(tmpl.initialization.as_deref());

            if let Some(init_tmpl) = init_tmpl {
                let path = expand_template(init_tmpl, &rep.id, rep.bandwidth, 0, None);
                return Ok((path, candidates, None));
            }
        }

        // ─ SegmentList init ──────────────────────────────────────────────────
        if let Some(sl) = merge_segment_lists(repr_obj, adaptation, period) {
            if let Some(src) = sl.Initialization.as_ref().and_then(|i| i.sourceURL.as_deref()) {
                return Ok((src.to_owned(), candidates, None));
            }
        }

        // ─ SegmentBase init ──────────────────────────────────────────────────
        // For SegmentBase, the init data is either:
        //   (a) a byte range within the media file declared by
        //       `SegmentBase.Initialization@range`, or
        //   (b) a separate URL given by `SegmentBase.Initialization.sourceURL`.
        if let Some(sb) = find_segment_base(repr_obj, adaptation, period) {
            // Check for Initialization@range (byte-range within the media file).
            if let Some(range_str) = sb.Initialization.as_ref().and_then(|i| i.range.as_deref()) {
                let br = parse_byte_range(range_str).ok_or(DashError::SegmentUrlUnresolvable)?;
                let media_path = segment_base_media_path(repr_obj, adaptation);
                return Ok((media_path, candidates, Some(br)));
            }

            // Check for Initialization.sourceURL (separate init file).
            if let Some(src) =
                sb.Initialization.as_ref().and_then(|i| i.sourceURL.as_deref())
            {
                return Ok((src.to_owned(), candidates, None));
            }
        }

        Err(DashError::SegmentUrlUnresolvable)
    }
}
