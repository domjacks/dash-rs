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

mod live;
mod mpd;
mod segment_url;
mod timeline;
mod utc;
mod xlink;

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use dash_mpd::{AdaptationSet, MPD, Period, Representation};
use tokio::sync::broadcast;
use url::Url;

use crate::{
    abr::{AbrController, AbrMetrics, QualityDecision, RepresentationInfo},
    error::DashError,
    fetch::{FetchRequest, FetchResponse, RequestType, SegmentFetcher},
    manifest::ManifestLoader,
    media_source::{
        sink::{SourceBufferSink, StreamInfo},
        Chunk, MediaSource,
    },
    mp4::{parse_all_emsg, parse_sidx, parse_sidx_entries, SidxEntry},
};

use self::{
    live::compute_live_edge_segment,
    mpd::{
        adaptation_mime_type, extract_representations, extract_segment_list_timing,
        extract_segment_timing, find_adaptation, find_segment_base, merge_segment_lists,
        merge_segment_templates, segment_base_media_path,
    },
    segment_url::{collect_candidate_base_urls, expand_template, resolve_effective_base_url},
    timeline::find_timeline_segment,
};

// ── MediaType ─────────────────────────────────────────────────────────────────

/// Which media track a session operation targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaType {
    Video,
    Audio,
}

impl std::fmt::Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
/// [`DashSession::subscribe_events`].  The channel is a
/// `tokio::sync::broadcast` channel with a buffer of 64 events; slow
/// consumers may see [`tokio::sync::broadcast::error::RecvError::Lagged`] if
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

/// Smoothing factor for the per-track throughput exponential moving average.
///
/// `α = 0.3` weights the newest sample at 30 % and the accumulated history at
/// 70 %, giving a half-life of roughly 1.9 segments.  This is intentionally
/// somewhat reactive: the first few segments after a network change will shift
/// the estimate meaningfully, while random per-segment variance is smoothed
/// out.  dash.js uses a pair of EWMAs (3 s and 8 s half-lives) and takes the
/// minimum; a single EMA with `α = 0.3` is a pragmatic starting point that
/// can be refined once real download timing data is available.
const THROUGHPUT_EMA_ALPHA: f64 = 0.3;

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
    /// `presentationTimeOffset` converted to seconds (0.0 when absent).
    timestamp_offset_s: f64,
}

/// Compute a [`TrackReinit`] from the given adaptation set and period.
///
/// This is a pure function with no side effects.  It replicates the
/// addressing-mode logic from [`DashSession::from_mpd_with_utc`] so that
/// period transitions can reuse the same code path.
fn compute_track_reinit(
    adaptation: &AdaptationSet,
    period: &Period,
    presentation_duration: Option<std::time::Duration>,
) -> TrackReinit {
    let representations = extract_representations(adaptation);
    let mime = adaptation_mime_type(adaptation);
    let first_repr = adaptation.representations.first();

    let (start_number_tmpl, seg_dur_ms_tmpl, total_tmpl) =
        extract_segment_timing(adaptation, period, presentation_duration);

    let tmpl = merge_segment_templates(first_repr, adaptation, period);
    let has_segment_list = merge_segment_lists(first_repr, adaptation, period).is_some();

    let timestamp_offset_s = tmpl.as_ref()
        .and_then(|t| {
            let pto = t.presentationTimeOffset?;
            let ts = t.timescale.unwrap_or(1);
            Some(pto as f64 / ts as f64)
        })
        .unwrap_or(0.0);

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
    }
}

/// Apply a [`TrackReinit`] to an existing [`TrackState`] in-place.
///
/// Updates the MIME type (via `change_type` if it changed), append window,
/// timestamp offset, and all timing/addressing fields.  Intentionally
/// preserves `throughput_ema_bps`, `last_transfer_ms`, and `current_index`
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
    /// Exponential moving average of measured download throughput in bits per
    /// second, updated after every successful media-segment fetch.
    ///
    /// `0.0` until the first segment has been fetched.  The smoothing factor
    /// is [`THROUGHPUT_EMA_ALPHA`].  Seeded directly from the first sample
    /// rather than from zero, to avoid the EMA taking many segments to climb
    /// out of a low initial value.
    throughput_ema_bps: f64,
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
}

impl TrackState {
    fn new(
        media_type: MediaType,
        representations: Vec<RepresentationInfo>,
        sink: SourceBufferSink,
        start_number: u64,
        segment_duration_ms: f64,
        total_segments: Option<u64>,
        addressing_mode: AddressingMode,
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
            throughput_ema_bps: 0.0,
            last_transfer_ms: 0.0,
            segment_byte_ranges: None,
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
/// let metrics = session.current_metrics(MediaType::Video);
/// let _ = session.fetch_next_segment(MediaType::Video, &metrics).await?;
/// # Ok(())
/// # }
/// ```
pub struct DashSession<F: SegmentFetcher, M: MediaSource> {
    mpd: MPD,
    base_url: Url,
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
    /// Wall-clock instant when the pipeline transitioned to Playing.
    playback_started_at: Option<Instant>,
    /// True while the pipeline is in the Paused state.
    paused: bool,
    /// Wall-clock instant when the pipeline entered Paused.
    paused_at: Option<Instant>,
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
    /// Wall-clock instant of the most recent manifest load or refresh.
    last_manifest_refresh: Option<Instant>,
    /// Cache of initialisation segment bytes, keyed by `"{media_type}/{rep_id}"`.
    ///
    /// Populated by [`Self::fetch_init_segment`].  On a quality switch inside
    /// [`Self::fetch_next_segment`], the new representation's init segment is
    /// re-appended from this cache without a network round-trip (dash.js
    /// `InitCache.js` equivalent).  A cache miss triggers a live fetch that
    /// also populates the cache entry for future switches.
    init_cache: HashMap<String, bytes::Bytes>,
    /// Signed offset in seconds applied to the wall clock before live-edge
    /// segment calculations: `adjusted_now = SystemTime::now() + utc_clock_offset_s`.
    ///
    /// Set to `server_time − client_time` by [`Self::from_url`] after querying
    /// the first successful `UTCTiming` element in the manifest.  Stays `0.0`
    /// when no `UTCTiming` is present, all elements fail, or the session was
    /// constructed via [`Self::from_mpd`] (no network access allowed).
    utc_clock_offset_s: f64,
    /// Broadcast sender for in-band DASH events extracted from `emsg` boxes.
    ///
    /// Callers subscribe via [`Self::subscribe_events`].  If no subscribers
    /// are present, sends are silently dropped (the `broadcast` channel
    /// discards messages when there are zero receivers).
    event_tx: broadcast::Sender<DashEvent>,
    /// Optional maximum bitrate cap for video, in bits per second.
    ///
    /// When set, [`Self::fetch_next_segment`] clamps the ABR decision to the
    /// highest representation whose bandwidth is at or below this limit.
    video_max_bitrate_bps: Option<u64>,
    /// Optional maximum bitrate cap for audio, in bits per second.
    audio_max_bitrate_bps: Option<u64>,
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
    pub async fn from_url(url: &Url, fetcher: F, media_source: M) -> Result<Self, DashError> {
        let fetcher = Arc::new(fetcher);
        let loader = ManifestLoader::new(Arc::clone(&fetcher));
        let (mpd, base_url) = loader.load(url).await?;

        // Resolve any eager XLink references (xlink:actuate="onLoad") so that
        // the rest of session construction sees a fully self-contained MPD.
        let mpd = xlink::resolve_xlinks(mpd, &base_url, &*fetcher).await?;

        // Compute UTC clock offset before constructing the session so that
        // live-edge snapping inside `from_mpd` can use the corrected time.
        let client_now_s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let utc_offset = utc::resolve_utc_timing(&mpd, &fetcher, client_now_s).await;

        let mut session = Self::from_mpd_with_utc(mpd, base_url, fetcher, media_source, utc_offset)?;
        session.last_manifest_refresh = Some(Instant::now());
        Ok(session)
    }

    /// Build a session from an already-parsed [`MPD`], without performing any
    /// network requests.
    ///
    /// The UTC clock offset is set to `0.0` — the system wall clock is used
    /// as-is for live-edge snapping.  Use [`Self::from_url`] to get automatic
    /// `UTCTiming` resolution.
    pub fn from_mpd(
        mpd: MPD,
        base_url: Url,
        fetcher: Arc<F>,
        media_source: M,
    ) -> Result<Self, DashError> {
        Self::from_mpd_with_utc(mpd, base_url, fetcher, media_source, 0.0)
    }

    /// Internal constructor shared by [`Self::from_url`] and [`Self::from_mpd`].
    fn from_mpd_with_utc(
        mpd: MPD,
        base_url: Url,
        fetcher: Arc<F>,
        mut media_source: M,
        utc_clock_offset_s: f64,
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
            if !reinit.representations.is_empty() {
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
                ))
            } else {
                None
            }
        } else {
            None
        };

        let mut audio_track = if let Some(adaptation) = audio_adaptation {
            let reinit = compute_track_reinit(adaptation, period, presentation_duration);
            if !reinit.representations.is_empty() {
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
                ))
            } else {
                None
            }
        } else {
            None
        };

        // Apply live-edge starting segment for dynamic streams.
        let is_dynamic = mpd.mpdtype.as_deref() == Some("dynamic");
        let ast_s: Option<f64> = mpd.availabilityStartTime.map(|dt| dt.timestamp() as f64);
        let spd_s: Option<f64> = mpd.suggestedPresentationDelay.map(|d| d.as_secs_f64());
        if is_dynamic {
            if let Some(ast) = ast_s {
                // Apply the UTC clock offset so that the live-edge calculation
                // uses the server's time rather than the (potentially skewed)
                // client wall clock.
                let now_s = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0)
                    + utc_clock_offset_s;
                if let Some(ref mut track) = video_track {
                    let seg = compute_live_edge_segment(
                        ast,
                        now_s,
                        track.segment_duration_ms / 1_000.0,
                        track.start_number,
                        spd_s,
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

        let mpd_minimum_update_period_ms = mpd.minimumUpdatePeriod.map(|d| d.as_millis() as u64);

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);

        Ok(Self {
            mpd,
            base_url,
            fetcher,
            media_source,
            abr: AbrController::default(),
            video_track,
            audio_track,
            current_period_index: 0,
            buffer_target_ms: 12_000.0,
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
            video_max_bitrate_bps: None,
            audio_max_bitrate_bps: None,
        })
    }

    // ── Introspection ─────────────────────────────────────────────────────────

    /// The parsed MPD manifest.
    pub fn mpd(&self) -> &MPD {
        &self.mpd
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
    /// `adjusted_now = SystemTime::now() + utc_clock_offset_s`.
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
    /// The returned [`broadcast::Receiver`] is a multi-producer,
    /// multi-consumer channel — multiple subscribers each receive every event
    /// independently.  If a subscriber's internal buffer fills up (capacity
    /// [`EVENT_CHANNEL_CAPACITY`] = 64), the oldest events are overwritten and
    /// the next `recv()` call will return
    /// [`tokio::sync::broadcast::error::RecvError::Lagged`].
    pub fn subscribe_events(&self) -> broadcast::Receiver<DashEvent> {
        self.event_tx.subscribe()
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
    pub fn current_metrics(&self, media_type: MediaType) -> AbrMetrics {
        let track = match media_type {
            MediaType::Video => self.video_track.as_ref(),
            MediaType::Audio => self.audio_track.as_ref(),
        };

        let (buffer_level_ms, current_index, throughput_bps, latency_ms) =
            if let Some(t) = track {
                let buffered = t.sink.buffered();
                let level = buffered.last().map(|r| r.end * 1_000.0).unwrap_or(0.0);
                (level, Some(t.current_index), t.throughput_ema_bps, t.last_transfer_ms)
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
                self.total_paused_ms += paused_at.elapsed().as_millis() as f64;
            }
            self.paused = false;
        } else {
            self.playback_started_at = Some(Instant::now());
            self.total_paused_ms = 0.0;
        }
    }

    /// Record that the pipeline has entered Paused state.
    pub fn notify_paused(&mut self) {
        if !self.paused {
            self.paused = true;
            self.paused_at = Some(Instant::now());
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
            .map(|started| {
                let total_elapsed = started.elapsed().as_millis() as f64;
                let current_pause_ms = if self.paused {
                    self.paused_at
                        .map(|pa| pa.elapsed().as_millis() as f64)
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
            .map(|started| {
                let total_elapsed = started.elapsed().as_millis() as f64;
                let current_pause_ms = if self.paused {
                    self.paused_at
                        .map(|pa| pa.elapsed().as_millis() as f64)
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

    // ── Live stream ───────────────────────────────────────────────────────────

    /// Returns `true` when the manifest describes a live/dynamic stream.
    pub fn is_live(&self) -> bool {
        self.mpd.mpdtype.as_deref() == Some("dynamic")
    }

    /// The minimum interval (in milliseconds) between manifest refreshes.
    pub fn minimum_update_period_ms(&self) -> Option<u64> {
        self.minimum_update_period_ms
    }

    /// Returns `true` when the session should call [`Self::refresh_manifest`]
    /// before the next segment fetch.
    pub fn should_refresh_manifest(&self) -> bool {
        if !self.is_live() {
            return false;
        }
        let Some(period_ms) = self.minimum_update_period_ms else {
            return false;
        };
        match self.last_manifest_refresh {
            None => true,
            Some(last) => last.elapsed().as_millis() as u64 >= period_ms,
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
    pub async fn refresh_manifest(&mut self) -> Result<(), DashError> {
        tracing::info!(url = %self.base_url, "refreshing live manifest");

        let loader = ManifestLoader::new(Arc::clone(&self.fetcher));
        let (new_mpd, new_base_url) = loader.load(&self.base_url).await?;

        self.last_manifest_refresh = Some(Instant::now());
        self.base_url = new_base_url;
        self.minimum_update_period_ms = new_mpd.minimumUpdatePeriod.map(|d| d.as_millis() as u64);

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
        let quality_switched = {
            let track = self.track_mut(&media_type)?;
            if decision.index != track.current_index {
                tracing::info!(
                    media_type = %media_type,
                    old = track.current_index,
                    new = decision.index,
                    reason = ?decision.reason,
                    "quality switch"
                );
                track.current_index = decision.index;
                self.abr.record_switch();
                true
            } else {
                false
            }
        };

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

                // `seg_num` is used as a direct 0-based index (start_number is 0).
                let seg_idx = seg_num as usize;
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
                let p = expand_template(media_tmpl, &rep.id, rep.bandwidth, 0, Some(seg_time));
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

        // Fetch — try each CDN candidate in priority order.
        let fetcher = Arc::clone(&self.fetcher);
        let abandon_ms = self.abandon_timeout_ms;
        let mt_str = media_type.to_string();

        let mut last_err: DashError = DashError::AllBaseUrlsFailed {
            last_error: "no candidates".into(),
        };
        let mut opt_response: Option<FetchResponse> = None;

        'candidates: for base in &candidates {
            let segment_url = base
                .join(&expanded_path)
                .map_err(|_| DashError::SegmentUrlUnresolvable)?;

            tracing::debug!(
                media_type = %media_type,
                segment_number = seg_number,
                url = %segment_url,
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
                match tokio::time::timeout(
                    std::time::Duration::from_millis(ms),
                    fetcher.fetch(request),
                )
                .await
                {
                    Ok(Ok(resp)) => Ok(resp),
                    Ok(Err(e)) => Err(e.into()),
                    Err(_elapsed) => {
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
                    tracing::warn!(
                        url = %segment_url,
                        error = %e,
                        "segment fetch failed — trying next base URL"
                    );
                    last_err = DashError::AllBaseUrlsFailed {
                        last_error: e.to_string(),
                    };
                }
            }
        }

        let response = opt_response.ok_or(last_err)?;

        // Update per-track throughput EMA and transfer latency from this response.
        {
            let track = self.track_mut(&media_type)?;
            track.last_transfer_ms = response.transfer_duration_ms as f64;
            if let Some(bps) = response.throughput_bps() {
                if track.throughput_ema_bps == 0.0 {
                    // First sample: seed the EMA directly so it does not spend
                    // many segments climbing out of an artificially low value.
                    track.throughput_ema_bps = bps;
                } else {
                    track.throughput_ema_bps = THROUGHPUT_EMA_ALPHA * bps
                        + (1.0 - THROUGHPUT_EMA_ALPHA) * track.throughput_ema_bps;
                }
            }
            tracing::debug!(
                media_type = %media_type,
                segment_number = seg_number,
                throughput_bps = track.throughput_ema_bps,
                transfer_ms = track.last_transfer_ms,
                "throughput EMA updated"
            );
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
                // Errors here mean no subscribers — that is fine.
                let _ = self.event_tx.send(event);
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
        if self.video_track.is_some() {
            self.fetch_init_segment(MediaType::Video).await?;
        }
        if self.audio_track.is_some() {
            self.fetch_init_segment(MediaType::Audio).await?;
        }

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
    pub async fn fetch_init_segment(&mut self, media_type: MediaType) -> Result<usize, DashError> {
        let (init_url, byte_range, rep_id) = {
            let track = self.track(&media_type)?;
            let rep = &track.representations[track.current_index];
            let (url, br) = self.resolve_init_url(&media_type, rep)?;
            (url, br, rep.id.clone())
        };

        tracing::debug!(
            media_type = %media_type,
            url = %init_url,
            byte_range = ?byte_range,
            "fetching init segment"
        );

        let request = {
            let req = FetchRequest::new(init_url, RequestType::InitializationSegment);
            if let Some((start, end)) = byte_range {
                req.with_byte_range(start, end)
            } else {
                req
            }
        };
        let response = self.fetcher.fetch(request).await?;
        let bytes_received = response.data.len();

        // Populate the cache so quality-switch re-appends are free.
        let cache_key = format!("{}/{}", media_type, rep_id);
        self.init_cache.insert(cache_key, response.data.clone());

        let chunk = Chunk::new(response.data, None, None, format!("{}/init", media_type));

        {
            let track = self.track_mut(&media_type)?;
            track.sink.append(chunk).await?;
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

    /// Resolve the URL (and optional byte range) for the initialisation
    /// segment of the given representation.
    ///
    /// Returns `(url, byte_range)` where `byte_range` is `Some((start, end))`
    /// when the init data is a byte range within the media file (as declared
    /// by `SegmentBase.Initialization@range`).
    fn resolve_init_url(
        &self,
        media_type: &MediaType,
        rep: &RepresentationInfo,
    ) -> Result<(Url, Option<(u64, u64)>), DashError> {
        let period = &self.mpd.periods[self.current_period_index];
        let adaptation =
            find_adaptation(period, media_type).ok_or_else(|| DashError::NoRepresentations {
                media_type: media_type.to_string(),
            })?;

        let repr_obj: Option<&Representation> = adaptation
            .representations
            .iter()
            .find(|r| r.id.as_deref() == Some(rep.id.as_str()));

        // ─ SegmentTemplate init ──────────────────────────────────────────────
        if let Some(tmpl) = merge_segment_templates(repr_obj, adaptation, period) {
            let init_tmpl = tmpl
                .initialization
                .as_deref()
                .or_else(|| tmpl.Initialization.as_ref()?.sourceURL.as_deref());

            if let Some(init_tmpl) = init_tmpl {
                let url_str = expand_template(init_tmpl, &rep.id, rep.bandwidth, 0, None);

                let repr_base_urls = repr_obj.map(|r| r.BaseURL.as_slice()).unwrap_or(&[]);
                let base = resolve_effective_base_url(
                    &self.base_url,
                    &self.mpd.base_url,
                    period,
                    adaptation,
                    repr_base_urls,
                );
                let url = base
                    .join(&url_str)
                    .map_err(|_| DashError::SegmentUrlUnresolvable)?;
                return Ok((url, None));
            }
        }

        // ─ SegmentList init ──────────────────────────────────────────────────
        if let Some(sl) = merge_segment_lists(repr_obj, adaptation, period) {
            if let Some(src) = sl.Initialization.as_ref().and_then(|i| i.sourceURL.as_deref()) {
                let repr_base_urls = repr_obj.map(|r| r.BaseURL.as_slice()).unwrap_or(&[]);
                let base = resolve_effective_base_url(
                    &self.base_url,
                    &self.mpd.base_url,
                    period,
                    adaptation,
                    repr_base_urls,
                );
                let url = base
                    .join(src)
                    .map_err(|_| DashError::SegmentUrlUnresolvable)?;
                return Ok((url, None));
            }
        }

        // ─ SegmentBase init ──────────────────────────────────────────────────
        // For SegmentBase, the init data is either:
        //   (a) a byte range within the media file declared by
        //       `SegmentBase.Initialization@range`, or
        //   (b) a separate URL given by `SegmentBase.Initialization.sourceURL`.
        if let Some(sb) = find_segment_base(repr_obj, adaptation, period) {
            let repr_base_urls = repr_obj.map(|r| r.BaseURL.as_slice()).unwrap_or(&[]);
            let base = resolve_effective_base_url(
                &self.base_url,
                &self.mpd.base_url,
                period,
                adaptation,
                repr_base_urls,
            );

            // Check for Initialization@range (byte-range within the media file).
            if let Some(range_str) = sb.Initialization.as_ref().and_then(|i| i.range.as_deref()) {
                let br = parse_byte_range(range_str).ok_or(DashError::SegmentUrlUnresolvable)?;
                // The file is the media URL resolved against the effective base.
                let media_path = segment_base_media_path(repr_obj, adaptation);
                let url = base
                    .join(&media_path)
                    .map_err(|_| DashError::SegmentUrlUnresolvable)?;
                return Ok((url, Some(br)));
            }

            // Check for Initialization.sourceURL (separate init file).
            if let Some(src) =
                sb.Initialization.as_ref().and_then(|i| i.sourceURL.as_deref())
            {
                let url = base
                    .join(src)
                    .map_err(|_| DashError::SegmentUrlUnresolvable)?;
                return Ok((url, None));
            }
        }

        Err(DashError::SegmentUrlUnresolvable)
    }
}
