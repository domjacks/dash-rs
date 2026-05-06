use std::{sync::Arc, time::Instant};

use dash_mpd::{AdaptationSet, BaseURL, Period, Representation, SegmentTemplate, MPD};
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
    mp4::parse_sidx,
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
    /// default 1).  Used to compute how many segments have been fetched.
    start_number: u64,
    /// Duration of a single segment in milliseconds, derived from
    /// `SegmentTemplate@duration / SegmentTemplate@timescale * 1000`.
    /// Used for buffer-level estimation.
    segment_duration_ms: f64,
    /// Total number of segments in this track, derived from
    /// `MPD@mediaPresentationDuration / segment_duration_ms`.
    /// `None` for live streams or manifests without a declared duration.
    total_segments: Option<u64>,
    /// The `SourceBufferSink` wrapping the underlying buffer.
    sink: SourceBufferSink,
}

impl TrackState {
    fn new(
        media_type: MediaType,
        representations: Vec<RepresentationInfo>,
        sink: SourceBufferSink,
        start_number: u64,
        segment_duration_ms: f64,
        total_segments: Option<u64>,
    ) -> Self {
        Self {
            _media_type: media_type,
            representations,
            current_index: 0,
            segment_number: start_number,
            start_number,
            segment_duration_ms,
            total_segments,
            sink,
        }
    }
}

// ── DashSession ───────────────────────────────────────────────────────────────

/// The top-level coordinator for a DASH playback session.
///
/// Mirrors the responsibilities shared between `MediaPlayer.js`,
/// `Stream.js`, and `StreamProcessor.js` in dash.js:
///
/// - Holds the parsed [`MPD`] manifest.
/// - Maintains per-track state (current quality, segment number, sink).
/// - Runs the [`AbrController`] to select quality for each fetch.
/// - Dispatches `FetchRequest`s via a [`SegmentFetcher`].
/// - Feeds fetched data into the appropriate [`SourceBufferSink`].
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
    /// Buffer target in milliseconds.  Fetch is gated until estimated buffer
    /// level drops below this value — mirrors `bufferTimeDefault` (12 s) in
    /// dash.js's `ScheduleController`.
    buffer_target_ms: f64,
    /// Wall-clock instant when the pipeline transitioned to Playing.  Set by
    /// [`Self::notify_playing`]; used for buffer-level estimation.
    playback_started_at: Option<Instant>,
    /// True while the pipeline is in the Paused state.
    paused: bool,
    /// Wall-clock instant when the pipeline entered Paused.  Used to track
    /// cumulative paused time so buffer-level estimates stay accurate.
    paused_at: Option<Instant>,
    /// Cumulative milliseconds spent in the Paused state since playback
    /// started.  Subtracted from the elapsed wall-clock time in
    /// [`Self::estimated_buffer_level_ms`].
    total_paused_ms: f64,
    /// When `true`, the fetch loop continues even while the pipeline is
    /// paused — mirrors `scheduleWhilePaused` in dash.js's
    /// `ScheduleController`.  Defaults to `false`.
    schedule_while_paused: bool,
    /// How much buffer (in seconds) to preserve behind the current playback
    /// position when [`Self::prune_all_tracks`] is called.  Default 30 s.
    buffer_to_keep_s: f64,
    /// Optional maximum duration (in milliseconds) to wait for a single
    /// segment fetch before returning [`DashError::SegmentAbandoned`].
    /// `None` disables the timeout.  Mirrors `AbandonRequestsRule`'s
    /// role in dash.js.
    abandon_timeout_ms: Option<u64>,
}

// Re-export for convenience in the example.
pub use crate::session::MediaType as SessionMediaType;

impl<F: SegmentFetcher, M: MediaSource> DashSession<F, M> {
    // ── Constructors ──────────────────────────────────────────────────────────

    /// Fetch the manifest at `url`, parse it, initialise source buffers on
    /// `media_source`, and return a ready-to-use session.
    pub async fn from_url(url: &Url, fetcher: F, media_source: M) -> Result<Self, DashError> {
        let fetcher = Arc::new(fetcher);
        let loader = ManifestLoader::new(Arc::clone(&fetcher));
        let (mpd, base_url) = loader.load(url).await?;

        let session = Self::from_mpd(mpd, base_url, fetcher, media_source)?;
        Ok(session)
    }

    /// Build a session from an already-parsed [`MPD`], without performing any
    /// network requests. Useful when the caller manages manifest loading itself.
    pub fn from_mpd(
        mpd: MPD,
        base_url: Url,
        fetcher: Arc<F>,
        mut media_source: M,
    ) -> Result<Self, DashError> {
        if mpd.periods.is_empty() {
            return Err(DashError::EmptyManifest);
        }

        // Use the first period for now (multi-period support can be added later).
        let period = &mpd.periods[0];
        let presentation_duration = mpd.mediaPresentationDuration;

        // Extract video and audio adaptation sets.
        // `period.adaptations` is a plain Vec (not Option<Vec>) in dash-mpd.
        let video_adaptation = period
            .adaptations
            .iter()
            .find(dash_mpd::is_video_adaptation);

        let audio_adaptation = period
            .adaptations
            .iter()
            .find(dash_mpd::is_audio_adaptation);

        let video_track = if let Some(adaptation) = video_adaptation {
            let reps = extract_representations(adaptation);
            if !reps.is_empty() {
                let mime = adaptation_mime_type(adaptation);
                let sink_buf = media_source.add_source_buffer(&mime)?;
                let mut sink = SourceBufferSink::new(sink_buf);

                // Wire append window from period/stream boundaries.
                let effective_duration = presentation_duration.or(period.duration);
                let stream_info = StreamInfo {
                    start: period.start.map(|d| d.as_secs_f64()).unwrap_or(0.0),
                    duration: effective_duration.map(|d| d.as_secs_f64()),
                };
                sink.update_append_window(
                    &stream_info,
                    presentation_duration.map(|d| d.as_secs_f64()),
                );

                // Wire timestamp offset from SegmentTemplate@presentationTimeOffset.
                // Merge across Representation → AdaptationSet → Period so that a
                // template defined only at the Representation level is found too.
                let first_repr = adaptation.representations.first();
                if let Some(merged) = merge_segment_templates(first_repr, adaptation, period) {
                    if let Some(pto) = merged.presentationTimeOffset {
                        let ts = merged.timescale.unwrap_or(1);
                        sink.set_timestamp_offset(pto as f64 / ts as f64);
                    }
                }

                let (start_number, seg_dur_ms, total_segments) =
                    extract_segment_timing(adaptation, period, presentation_duration);
                Some(TrackState::new(
                    MediaType::Video,
                    reps,
                    sink,
                    start_number,
                    seg_dur_ms,
                    total_segments,
                ))
            } else {
                None
            }
        } else {
            None
        };

        let audio_track = if let Some(adaptation) = audio_adaptation {
            let reps = extract_representations(adaptation);
            if !reps.is_empty() {
                let mime = adaptation_mime_type(adaptation);
                let sink_buf = media_source.add_source_buffer(&mime)?;
                let mut sink = SourceBufferSink::new(sink_buf);

                // Wire append window from period/stream boundaries.
                let effective_duration = presentation_duration.or(period.duration);
                let stream_info = StreamInfo {
                    start: period.start.map(|d| d.as_secs_f64()).unwrap_or(0.0),
                    duration: effective_duration.map(|d| d.as_secs_f64()),
                };
                sink.update_append_window(
                    &stream_info,
                    presentation_duration.map(|d| d.as_secs_f64()),
                );

                // Wire timestamp offset from SegmentTemplate@presentationTimeOffset.
                // Merge across Representation → AdaptationSet → Period so that a
                // template defined only at the Representation level is found too.
                let first_repr = adaptation.representations.first();
                if let Some(merged) = merge_segment_templates(first_repr, adaptation, period) {
                    if let Some(pto) = merged.presentationTimeOffset {
                        let ts = merged.timescale.unwrap_or(1);
                        sink.set_timestamp_offset(pto as f64 / ts as f64);
                    }
                }

                let (start_number, seg_dur_ms, total_segments) =
                    extract_segment_timing(adaptation, period, presentation_duration);
                Some(TrackState::new(
                    MediaType::Audio,
                    reps,
                    sink,
                    start_number,
                    seg_dur_ms,
                    total_segments,
                ))
            } else {
                None
            }
        } else {
            None
        };

        Ok(Self {
            mpd,
            base_url,
            fetcher,
            media_source,
            abr: AbrController::default(),
            video_track,
            audio_track,
            buffer_target_ms: 12_000.0,
            playback_started_at: None,
            paused: false,
            paused_at: None,
            total_paused_ms: 0.0,
            schedule_while_paused: false,
            buffer_to_keep_s: 30.0,
            abandon_timeout_ms: None,
        })
    }

    // ── Introspection ─────────────────────────────────────────────────────────

    /// The parsed MPD manifest.
    pub fn mpd(&self) -> &MPD {
        &self.mpd
    }

    /// Shared reference to the underlying [`MediaSource`].
    ///
    /// Useful for accessing implementation-specific methods (e.g.
    /// `GstMediaSource::set_playing`) after the session has been constructed.
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
    /// current session state. The caller should supply real throughput and
    /// buffer measurements; here we use zeroed placeholders.
    pub fn current_metrics(&self, media_type: MediaType) -> AbrMetrics {
        let track = match media_type {
            MediaType::Video => self.video_track.as_ref(),
            MediaType::Audio => self.audio_track.as_ref(),
        };

        let (buffer_level_ms, current_index) = if let Some(t) = track {
            let buffered = t.sink.buffered();
            let level = buffered.last().map(|r| r.end * 1_000.0).unwrap_or(0.0);
            (level, Some(t.current_index))
        } else {
            (0.0, None)
        };

        AbrMetrics {
            throughput_bps: 0.0,
            buffer_level_ms,
            latency_ms: 0.0,
            current_index,
            dropped_frames: None,
            total_decoded_frames: None,
            effective_download_ratio: None,
        }
    }

    // ── Buffer scheduling ─────────────────────────────────────────────────────

    /// Record that the pipeline has entered Playing state.
    ///
    /// - On the **first** call (before any pause): records the wall-clock
    ///   instant that playback started so [`Self::estimated_buffer_level_ms`]
    ///   can subtract elapsed playing time.
    /// - On subsequent calls after a pause: accumulates the paused duration
    ///   into `total_paused_ms` and marks the session as no longer paused.
    ///
    /// Mirrors the way dash.js reacts to `PLAYBACK_PLAYING` events in
    /// `ScheduleController`.
    pub fn notify_playing(&mut self) {
        if self.paused {
            // Resuming from pause: accumulate the time spent paused.
            if let Some(paused_at) = self.paused_at.take() {
                self.total_paused_ms += paused_at.elapsed().as_millis() as f64;
            }
            self.paused = false;
        } else {
            // First transition to Playing: record the start instant.
            self.playback_started_at = Some(Instant::now());
            self.total_paused_ms = 0.0;
        }
    }

    /// Record that the pipeline has entered Paused state.
    ///
    /// Freezes the wall-clock used by [`Self::estimated_buffer_level_ms`] so
    /// that buffer estimates do not drift while the player is paused.
    /// Call [`Self::notify_playing`] to resume the clock.
    ///
    /// Mirrors the way dash.js reacts to `PLAYBACK_PAUSED` events in
    /// `ScheduleController`.
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

    /// Whether the fetch loop should continue to run while the pipeline is
    /// paused.  Mirrors `scheduleWhilePaused` in dash.js's
    /// `ScheduleController`.  Default `false`.
    pub fn schedule_while_paused(&self) -> bool {
        self.schedule_while_paused
    }

    /// Set whether the fetch loop continues during pause.
    pub fn set_schedule_while_paused(&mut self, v: bool) {
        self.schedule_while_paused = v;
    }

    /// Estimated buffer level ahead of the current playback position, in
    /// milliseconds.
    ///
    /// Computed as `segments_fetched × segment_duration_ms − elapsed_playing_ms`.
    /// This is a wall-clock approximation: it assumes the pipeline plays at
    /// exactly real-time speed, which is true for a non-live pipeline with
    /// correct timestamps.
    ///
    /// Time spent paused is subtracted so that pausing does not inflate the
    /// reported buffer level.
    ///
    /// Returns `0.0` before any segments have been fetched or if the estimate
    /// would be negative (buffer empty / stall condition).
    pub fn estimated_buffer_level_ms(&self, media_type: &MediaType) -> f64 {
        let track = match self.track(media_type) {
            Ok(t) => t,
            Err(_) => return 0.0,
        };
        let segments_fetched = track.segment_number.saturating_sub(track.start_number);
        let pushed_ms = segments_fetched as f64 * track.segment_duration_ms;
        let elapsed_ms = self
            .playback_started_at
            .map(|started| {
                let total_elapsed = started.elapsed().as_millis() as f64;
                // Subtract time spent paused, including any ongoing pause.
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
    ///
    /// Useful as a coarse `current_time` value for [`Self::prune_all_tracks`]
    /// when the caller does not have direct access to the renderer position
    /// (e.g. from a background async task that owns the session but cannot
    /// query GStreamer's pipeline position).
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

    /// The buffer fill target in milliseconds.
    ///
    /// The scheduling loop should gate fetches until the estimated buffer level
    /// falls below this value.  Default: **12 000 ms** (matches
    /// `bufferTimeDefault` in dash.js's `ScheduleController`).
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

    /// The segment-fetch abandon timeout in milliseconds, or `None` if
    /// disabled.
    pub fn abandon_timeout_ms(&self) -> Option<u64> {
        self.abandon_timeout_ms
    }

    /// Set (or clear) the per-segment fetch timeout.
    ///
    /// When set, each segment fetch that does not complete within `ms`
    /// milliseconds returns [`DashError::SegmentAbandoned`].  The caller
    /// should then lower quality and retry at the next opportunity.
    pub fn set_abandon_timeout_ms(&mut self, ms: Option<u64>) {
        self.abandon_timeout_ms = ms;
    }

    // ── Buffer pruning ────────────────────────────────────────────────────────

    /// Remove buffered data older than `current_time_s − buffer_to_keep_behind_s`
    /// from all tracks.
    ///
    /// Call this periodically from the fetch loop (after each segment or on a
    /// timer) to prevent unbounded memory growth.  Mirrors
    /// `BufferController.pruneBuffer()` / `SourceBufferSink.remove()` in
    /// dash.js.
    ///
    /// `current_time_s` — the current playback position in seconds.  Use
    /// [`Self::estimated_playback_position_s`] if no renderer-provided value
    /// is available.
    ///
    /// `buffer_to_keep_behind_s` — how many seconds of data to retain behind
    /// the playback position (default comes from [`Self::buffer_to_keep_s`]).
    ///
    /// Does nothing (returns `Ok(())`) when `current_time_s − buffer_to_keep_behind_s ≤ 0`.
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

    // ── ABR ───────────────────────────────────────────────────────────────────

    /// Run the ABR controller and return the recommended quality for the
    /// given media type without changing any state.
    pub fn select_quality(
        &self,
        media_type: &MediaType,
        metrics: &AbrMetrics,
    ) -> Result<QualityDecision, DashError> {
        let track = self.track(media_type)?;
        Ok(self.abr.select(&track.representations, metrics))
    }

    /// Explicitly update the metrics and re-run ABR, applying the decision to
    /// the current session state.
    pub fn update_metrics(&mut self, media_type: &MediaType, metrics: AbrMetrics) {
        // Clone representations to release the immutable borrow before taking
        // a mutable one for the quality switch.
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
        // Record the switch in the history rule.
        self.abr.record_switch();
    }

    // ── Fetching ──────────────────────────────────────────────────────────────

    /// Fetch the next segment for the given media type using the current ABR
    /// quality decision, then append it to the corresponding source buffer.
    ///
    /// The `metrics` parameter should reflect the most recent throughput and
    /// buffer measurements so the ABR rules can react.
    ///
    /// If [`Self::set_abandon_timeout_ms`] is configured and the fetch does
    /// not complete in time, returns [`DashError::SegmentAbandoned`].
    ///
    /// Multiple `BaseURL` candidates at the `Period` and `AdaptationSet`
    /// levels are tried in priority order (per DASH spec §5.10.1.3) with
    /// weight-based random ordering within each priority tier.  If every
    /// candidate fails, returns [`DashError::AllBaseUrlsFailed`].
    ///
    /// Returns the [`QualityDecision`] that was applied, or
    /// [`DashError::EndOfStream`] when all segments declared by
    /// `MPD@mediaPresentationDuration` have been fetched.
    pub async fn fetch_next_segment(
        &mut self,
        media_type: MediaType,
        metrics: &AbrMetrics,
    ) -> Result<QualityDecision, DashError> {
        // Check whether all segments have been consumed.
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

        // Time-based EOS guard (mirrors dash.js ScheduleController):
        //   signal EndOfStream when (timeToStreamEnd − bufferLevel) < ε
        // This fires when the buffer already covers the tail of the stream,
        // even before every declared segment has been individually counted.
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
            self.abr.select(&track.representations, metrics)
        };

        // Apply quality switch if needed.
        {
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
            }
        }

        // Build the expanded segment path and collect CDN candidate base URLs.
        // All borrows of `self` are released before the async fetch loop.
        let (candidates, expanded_path, seg_number) = {
            let track = self.track(&media_type)?;
            let rep = &track.representations[track.current_index];
            let seg_num = track.segment_number;

            let period = &self.mpd.periods[0];
            let adaptation = find_adaptation(period, &media_type).ok_or_else(|| {
                DashError::NoRepresentations {
                    media_type: media_type.to_string(),
                }
            })?;

            // Find the dash_mpd::Representation for the selected quality level
            // so we can check Representation-level SegmentTemplate and BaseURL.
            let repr_obj: Option<&Representation> = adaptation
                .representations
                .iter()
                .find(|r| r.id.as_deref() == Some(rep.id.as_str()));

            // Merge SegmentTemplate: Representation → AdaptationSet → Period.
            // Handles manifests (e.g. USP / Unified Streaming) where the template
            // is at the Representation level rather than the AdaptationSet level.
            let merged = merge_segment_templates(repr_obj, adaptation, period)
                .ok_or(DashError::SegmentUrlUnresolvable)?;

            let media_tmpl = merged
                .media
                .as_deref()
                .ok_or(DashError::SegmentUrlUnresolvable)?;

            let path = expand_template(media_tmpl, &rep.id, rep.bandwidth, seg_num);

            // Collect CDN candidates across all four BaseURL levels:
            // manifest URL → MPD → Period → AdaptationSet → Representation.
            let repr_base_urls = repr_obj.map(|r| r.BaseURL.as_slice()).unwrap_or(&[]);
            let candidates = collect_candidate_base_urls(
                &self.base_url,
                &self.mpd.base_url,
                period,
                adaptation,
                repr_base_urls,
            );
            (candidates, path, seg_num)
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

            let request = FetchRequest::new(segment_url.clone(), RequestType::MediaSegment);

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

        // Record throughput measurement.
        if let Some(bps) = response.throughput_bps() {
            tracing::debug!(throughput_bps = bps, "measured throughput");
        }

        // Build chunk and append.
        // Parse sidx timing from the fetched data before consuming it.
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
        }

        Ok(decision)
    }

    /// Fetch and append the initialisation segment for the given media type.
    ///
    /// Must be called once per track before [`fetch_next_segment`] is invoked
    /// so that a real decoder can parse codec parameters from the `moov` box.
    /// Returns the number of bytes received.
    ///
    /// Returns [`DashError::SegmentUrlUnresolvable`] if the manifest does not
    /// contain an `initialization` template for the requested track.
    pub async fn fetch_init_segment(&mut self, media_type: MediaType) -> Result<usize, DashError> {
        let init_url = {
            let track = self.track(&media_type)?;
            let rep = &track.representations[track.current_index];
            self.resolve_init_url(&media_type, rep)?
        };

        tracing::debug!(
            media_type = %media_type,
            url = %init_url,
            "fetching init segment"
        );

        let request = FetchRequest::new(init_url, RequestType::InitializationSegment);
        let response = self.fetcher.fetch(request).await?;
        let bytes_received = response.data.len();

        // Init segments carry no presentation timestamp; start/end are None.
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

    /// Resolve the URL for the initialisation segment of the given
    /// representation.
    ///
    /// Checks the merged `SegmentTemplate` across Representation → AdaptationSet
    /// → Period.  Accepts both the common `@initialization` attribute form and
    /// the `<Initialization sourceURL="…"/>` child-element form.
    /// Returns [`DashError::SegmentUrlUnresolvable`] when no initialisation
    /// URL can be derived from the manifest.
    fn resolve_init_url(
        &self,
        media_type: &MediaType,
        rep: &RepresentationInfo,
    ) -> Result<Url, DashError> {
        let period = &self.mpd.periods[0];
        let adaptation =
            find_adaptation(period, media_type).ok_or_else(|| DashError::NoRepresentations {
                media_type: media_type.to_string(),
            })?;

        // Find the dash_mpd::Representation to check Representation-level fields.
        let repr_obj: Option<&Representation> = adaptation
            .representations
            .iter()
            .find(|r| r.id.as_deref() == Some(rep.id.as_str()));

        // Merge SegmentTemplate: Representation → AdaptationSet → Period.
        if let Some(tmpl) = merge_segment_templates(repr_obj, adaptation, period) {
            // Prefer @initialization attribute; fall back to <Initialization sourceURL="...">.
            let init_tmpl = tmpl
                .initialization
                .as_deref()
                .or_else(|| tmpl.Initialization.as_ref()?.sourceURL.as_deref());

            if let Some(init_tmpl) = init_tmpl {
                let url_str = expand_template(init_tmpl, &rep.id, rep.bandwidth, 0);

                let repr_base_urls = repr_obj.map(|r| r.BaseURL.as_slice()).unwrap_or(&[]);
                let base = resolve_effective_base_url(
                    &self.base_url,
                    &self.mpd.base_url,
                    period,
                    adaptation,
                    repr_base_urls,
                );
                return base
                    .join(&url_str)
                    .map_err(|_| DashError::SegmentUrlUnresolvable);
            }
        }

        Err(DashError::SegmentUrlUnresolvable)
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// Extract `(start_number, segment_duration_ms, total_segments)` from the
/// `SegmentTemplate` that governs `adaptation`, walking up to `period` if
/// needed.  `presentation_duration` is `MPD@mediaPresentationDuration`.
///
/// `start_number` defaults to 1 when `@startNumber` is absent (DASH spec §5.3.9.2).
/// `segment_duration_ms` defaults to 2 000 ms when `@duration` / `@timescale`
/// are absent (safe fallback that keeps the buffer estimator conservative).
/// `total_segments` is derived from `presentation_duration` when set;
/// falls back to `Period@duration` when the MPD-level attribute is absent;
/// is `None` only when both are absent (e.g. live / dynamic stream).
fn extract_segment_timing(
    adaptation: &AdaptationSet,
    period: &Period,
    presentation_duration: Option<std::time::Duration>,
) -> (u64, f64, Option<u64>) {
    // Merge SegmentTemplate across hierarchy.  This function is called once
    // per adaptation set before a specific representation is chosen, so we
    // use the first representation as a sample.  All representations within
    // a track should have the same effective segment duration; we warn if they
    // differ since that would make the single `segment_duration_ms` field
    // inaccurate for some representations.
    let first_repr = adaptation.representations.first();
    let Some(t) = merge_segment_templates(first_repr, adaptation, period) else {
        return (1, 2_000.0, None);
    };

    // Warn if sibling representations declare inconsistent effective durations.
    if adaptation.representations.len() > 1 {
        let effective_dur_s = |r: &Representation| -> Option<f64> {
            merge_segment_templates(Some(r), adaptation, period)
                .and_then(|m| Some(m.duration? / m.timescale.unwrap_or(1) as f64))
        };
        let first_dur = first_repr.and_then(effective_dur_s);
        for r in adaptation.representations.iter().skip(1) {
            match (first_dur, effective_dur_s(r)) {
                (Some(fd), Some(rd)) if (fd - rd).abs() > 1e-9 => {
                    tracing::warn!(
                        "AdaptationSet representations have inconsistent segment durations \
                         ({fd} s vs {rd} s); using first representation as representative"
                    );
                    break;
                }
                _ => {}
            }
        }
    }

    let start_number = t.startNumber.unwrap_or(1);

    let seg_dur_ms = match (t.duration, t.timescale) {
        (Some(dur), Some(ts)) => dur / (ts as f64) * 1_000.0,
        (Some(dur), None) => dur * 1_000.0, // timescale defaults to 1
        _ => 2_000.0,
    };

    // Prefer MPD@mediaPresentationDuration; fall back to Period@duration so
    // that streams which omit the top-level attribute but declare a period
    // length still get a finite segment count.
    let effective_duration = presentation_duration.or(period.duration);

    let total_segments = effective_duration.map(|d| {
        let total_ms = d.as_millis() as f64;
        (total_ms / seg_dur_ms).ceil() as u64
    });

    (start_number, seg_dur_ms, total_segments)
}

fn extract_representations(adaptation: &AdaptationSet) -> Vec<RepresentationInfo> {
    let mut reps: Vec<RepresentationInfo> = adaptation
        .representations
        .iter()
        .filter_map(|r| {
            let bandwidth = r.bandwidth?;
            Some(RepresentationInfo {
                id: r.id.clone().unwrap_or_default(),
                bandwidth,
                width: r.width.map(|w| w as u32),
                height: r.height.map(|h| h as u32),
            })
        })
        .collect();

    reps.sort_by_key(|r| r.bandwidth);
    reps
}

/// Derive a MIME type string suitable for `addSourceBuffer` from an
/// [`AdaptationSet`].
fn adaptation_mime_type(adaptation: &AdaptationSet) -> String {
    // Prefer the explicit mimeType attribute on the AdaptationSet.
    if let Some(mime) = &adaptation.mimeType {
        // Attach codecs from the first representation if available.
        if let Some(rep) = adaptation.representations.first() {
            if let Some(codecs) = &rep.codecs {
                return format!("{};codecs=\"{}\"", mime, codecs);
            }
        }
        return mime.clone();
    }

    // Fall back to content-type derived from the first representation's
    // mimeType attribute (dash-mpd parses this per-representation too).
    if let Some(rep) = adaptation.representations.first() {
        if let Some(mime) = &rep.mimeType {
            if let Some(codecs) = &rep.codecs {
                return format!("{};codecs=\"{}\"", mime, codecs);
            }
            return mime.clone();
        }
    }

    // Last resort — opaque binary type.
    "application/octet-stream".to_owned()
}

/// Expand DASH `SegmentTemplate` identifier tokens.
///
/// Handled identifiers:
/// - `$RepresentationID$`
/// - `$Bandwidth$`
/// - `$Number$` and `$Number%0{width}d$` (zero-padded decimal)
///
/// Unknown identifiers are reproduced verbatim (including the surrounding
/// `$` delimiters) so as not to silently corrupt a URL.
fn expand_template(tmpl: &str, rep_id: &str, bandwidth: u64, number: u64) -> String {
    let mut out = String::with_capacity(tmpl.len() + 32);
    let mut s = tmpl;

    while let Some(open) = s.find('$') {
        out.push_str(&s[..open]);
        s = &s[open + 1..]; // skip opening '$'

        if let Some(close) = s.find('$') {
            let token = &s[..close]; // e.g. "Number%06d", "RepresentationID"
            s = &s[close + 1..]; // skip closing '$'

            // Split token into identifier and optional format specifier.
            let (name, fmt) = if let Some(pct) = token.find('%') {
                (&token[..pct], Some(&token[pct..]))
            } else {
                (token, None)
            };

            match name {
                "RepresentationID" => out.push_str(rep_id),
                "Bandwidth" => out.push_str(&bandwidth.to_string()),
                "Number" => {
                    // Handle %0{width}d zero-padding, e.g. %06d → "000001".
                    let mut formatted = false;
                    if let Some(f) = fmt {
                        if f.starts_with("%0") && f.ends_with('d') {
                            if let Ok(w) = f[2..f.len() - 1].parse::<usize>() {
                                out.push_str(&format!("{:0>width$}", number, width = w));
                                formatted = true;
                            }
                        }
                    }
                    if !formatted {
                        out.push_str(&number.to_string());
                    }
                }
                _ => {
                    // Unknown identifier — reproduce verbatim.
                    out.push('$');
                    out.push_str(token);
                    out.push('$');
                }
            }
        } else {
            // Unterminated '$' — pass through literally.
            out.push('$');
        }
    }

    out.push_str(s);
    out
}

/// Return a reference to the first adaptation set matching `media_type`
/// within `period`.
fn find_adaptation<'a>(period: &'a Period, media_type: &MediaType) -> Option<&'a AdaptationSet> {
    period.adaptations.iter().find(|a| match media_type {
        MediaType::Video => dash_mpd::is_video_adaptation(a),
        MediaType::Audio => dash_mpd::is_audio_adaptation(a),
    })
}

/// Collect candidate base URLs for a segment fetch by chaining `BaseURL`
/// elements at every level of the DASH hierarchy (DASH spec §5.10.1.3):
///
///   manifest URL → MPD → Period → AdaptationSet → Representation
///
/// At each level that declares `BaseURL` elements, each parent candidate is
/// combined with each child `BaseURL` (in priority / weight order) to produce
/// the next level's candidates.  Levels with no `BaseURL` elements are passed
/// through unchanged.
///
/// The list is guaranteed to be non-empty (falls back to the manifest URL).
fn collect_candidate_base_urls(
    manifest_url: &Url,
    mpd_base_urls: &[BaseURL],
    period: &Period,
    adaptation: &AdaptationSet,
    repr_base_urls: &[BaseURL],
) -> Vec<Url> {
    // Helper: expand a set of parent candidates with a set of BaseURL entries.
    fn expand(parents: Vec<Url>, bases: &[BaseURL]) -> Vec<Url> {
        if bases.is_empty() {
            return parents;
        }
        let ordered = priority_weighted_order(bases);
        parents
            .iter()
            .flat_map(|p| ordered.iter().filter_map(move |b| p.join(&b.base).ok()))
            .collect()
    }

    let root = vec![manifest_url.clone()];
    let after_mpd = expand(root, mpd_base_urls);
    let after_period = expand(after_mpd, &period.BaseURL);
    let after_adapt = expand(after_period, &adaptation.BaseURL);
    expand(after_adapt, repr_base_urls)
}

/// Sort a slice of `BaseURL` elements by priority ascending with
/// weight-proportional random ordering within each same-priority tier.
///
/// Uses the Efraimidis–Spirakis algorithm: each item receives a score
/// `U^(1/weight)` where U is uniform random; higher scores come first.
/// This produces a correctly weighted random permutation within each tier.
fn priority_weighted_order(bases: &[BaseURL]) -> Vec<&BaseURL> {
    use rand::Rng;
    let mut rng = rand::thread_rng();

    let mut scored: Vec<(u64, f64, &BaseURL)> = bases
        .iter()
        .map(|b| {
            let priority = b.priority.unwrap_or(1);
            let weight = b.weight.unwrap_or(1).max(1) as f64;
            // Avoid log(0): clamp U away from zero.
            let u: f64 = rng.gen::<f64>().max(1e-10_f64);
            let score = u.powf(1.0 / weight);
            (priority, score, b)
        })
        .collect();

    // Sort by priority ascending, then score descending within same priority.
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    scored.into_iter().map(|(_, _, b)| b).collect()
}

/// Resolve a single effective base URL for init-segment URL construction by
/// deterministically walking the `BaseURL` chain from the manifest URL through
/// every hierarchy level (MPD → Period → AdaptationSet → Representation).
///
/// Unlike [`collect_candidate_base_urls`], which produces a full candidate
/// list for CDN failover, this picks the **first** (highest-priority) element
/// at each level and chains them.  Suitable for one-shot URL resolution where
/// a single base is required.
fn resolve_effective_base_url(
    manifest_url: &Url,
    mpd_base_urls: &[BaseURL],
    period: &Period,
    adaptation: &AdaptationSet,
    repr_base_urls: &[BaseURL],
) -> Url {
    let mut base = manifest_url.clone();
    for level in [mpd_base_urls, &period.BaseURL, &adaptation.BaseURL, repr_base_urls] {
        if let Some(b) = level.first() {
            if let Ok(u) = base.join(&b.base) {
                base = u;
            }
        }
    }
    base
}

/// Merge `SegmentTemplate` attributes across the DASH element hierarchy.
///
/// DASH §5.3.9.4 specifies that attributes not set at a lower level shall be
/// inherited from the nearest parent element that defines them.  The
/// resolution order is Representation (most specific) → AdaptationSet → Period
/// (least specific).
///
/// Returns an **owned** merged template so the caller can hold it without
/// lifetime ties to multiple levels of the MPD tree.  Returns `None` when no
/// `SegmentTemplate` exists at any level.
fn merge_segment_templates(
    repr: Option<&Representation>,
    adaptation: &AdaptationSet,
    period: &Period,
) -> Option<SegmentTemplate> {
    // Collect sources from most-specific to least-specific.
    let sources: [Option<&SegmentTemplate>; 3] = [
        repr.and_then(|r| r.SegmentTemplate.as_ref()),
        adaptation.SegmentTemplate.as_ref(),
        period.SegmentTemplate.as_ref(),
    ];

    if sources.iter().all(|s| s.is_none()) {
        return None;
    }

    // For each attribute: first source that has a value wins.
    let mut merged = SegmentTemplate::default();
    for tmpl in sources.iter().flatten() {
        if merged.media.is_none() {
            merged.media.clone_from(&tmpl.media);
        }
        if merged.initialization.is_none() {
            merged.initialization.clone_from(&tmpl.initialization);
        }
        if merged.startNumber.is_none() {
            merged.startNumber = tmpl.startNumber;
        }
        if merged.duration.is_none() {
            merged.duration = tmpl.duration;
        }
        if merged.timescale.is_none() {
            merged.timescale = tmpl.timescale;
        }
        if merged.presentationTimeOffset.is_none() {
            merged.presentationTimeOffset = tmpl.presentationTimeOffset;
        }
        if merged.Initialization.is_none() {
            merged.Initialization.clone_from(&tmpl.Initialization);
        }
        if merged.SegmentTimeline.is_none() {
            merged.SegmentTimeline.clone_from(&tmpl.SegmentTimeline);
        }
    }

    Some(merged)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{expand_template, merge_segment_templates};
    use dash_mpd::{AdaptationSet, Period, Representation, SegmentTemplate};

    // ── merge_segment_templates ───────────────────────────────────────────────

    /// Template present only at the Representation level (no AdaptationSet or
    /// Period template).  Used by USP / Unified Streaming manifests where each
    /// Representation carries its own SegmentTemplate.
    #[test]
    fn merge_repr_level_only() {
        let mut repr = Representation::default();
        repr.SegmentTemplate = Some(SegmentTemplate {
            media: Some("video-$RepresentationID$-$Number$.m4s".into()),
            initialization: Some("video-$RepresentationID$-init.mp4".into()),
            timescale: Some(25_000),
            duration: Some(96_000.0),
            ..Default::default()
        });

        let merged =
            merge_segment_templates(Some(&repr), &AdaptationSet::default(), &Period::default())
                .unwrap();

        assert_eq!(
            merged.media.as_deref(),
            Some("video-$RepresentationID$-$Number$.m4s")
        );
        assert_eq!(merged.timescale, Some(25_000));
        assert_eq!(merged.duration, Some(96_000.0));
        assert_eq!(merged.startNumber, None); // not set → absent in merged
    }

    /// Attribute inheritance: `startNumber` lives on the AdaptationSet template,
    /// while `media`, `timescale`, and `duration` are on the Representation.
    /// The Representation's `timescale` should win over the AdaptationSet's.
    #[test]
    fn merge_inherits_from_adaptation_set() {
        let mut repr = Representation::default();
        repr.SegmentTemplate = Some(SegmentTemplate {
            media: Some("seg-$Number$.m4s".into()),
            timescale: Some(48_000),
            duration: Some(192_000.0),
            ..Default::default()
        });

        let mut adaptation = AdaptationSet::default();
        adaptation.SegmentTemplate = Some(SegmentTemplate {
            startNumber: Some(5),
            timescale: Some(1_000), // lower-priority; should be overridden
            ..Default::default()
        });

        let merged = merge_segment_templates(Some(&repr), &adaptation, &Period::default()).unwrap();

        assert_eq!(merged.media.as_deref(), Some("seg-$Number$.m4s"));
        // Representation timescale (48 000) wins over AdaptationSet (1 000).
        assert_eq!(merged.timescale, Some(48_000));
        // startNumber: inherited from AdaptationSet because Representation doesn't set it.
        assert_eq!(merged.startNumber, Some(5));
    }

    /// Period-level fallback: template only on Period, none lower.
    #[test]
    fn merge_period_level_fallback() {
        let mut period = Period::default();
        period.SegmentTemplate = Some(SegmentTemplate {
            media: Some("s$Number$.m4s".into()),
            timescale: Some(1_000),
            duration: Some(2_000.0),
            ..Default::default()
        });

        let merged =
            merge_segment_templates(None, &AdaptationSet::default(), &period).unwrap();

        assert_eq!(merged.media.as_deref(), Some("s$Number$.m4s"));
        assert_eq!(merged.timescale, Some(1_000));
    }

    /// No SegmentTemplate at any level → `None`.
    #[test]
    fn merge_no_template_returns_none() {
        assert!(merge_segment_templates(
            Some(&Representation::default()),
            &AdaptationSet::default(),
            &Period::default()
        )
        .is_none());
        assert!(merge_segment_templates(None, &AdaptationSet::default(), &Period::default())
            .is_none());
    }

    // ── expand_template ───────────────────────────────────────────────────────

    #[test]
    fn expand_plain_number() {
        assert_eq!(
            expand_template("seg_$Number$.m4s", "r1", 1000, 42),
            "seg_42.m4s"
        );
    }

    #[test]
    fn expand_zero_padded_number_width6() {
        // BBC R&D BBB uses %06d.
        assert_eq!(
            expand_template(
                "$RepresentationID$/$Number%06d$.m4s",
                "1920x1080p25",
                4741120,
                1
            ),
            "1920x1080p25/000001.m4s"
        );
    }

    #[test]
    fn expand_zero_padded_number_width3() {
        assert_eq!(expand_template("$Number%03d$.m4s", "id", 999, 7), "007.m4s");
    }

    #[test]
    fn expand_representation_id_and_bandwidth() {
        assert_eq!(
            expand_template("$RepresentationID$_$Bandwidth$.mp4", "hd", 5000000, 0),
            "hd_5000000.mp4"
        );
    }

    #[test]
    fn expand_unknown_identifier_preserved() {
        // $Time$ is not yet implemented; must not silently drop it.
        assert_eq!(expand_template("$Time$.m4s", "id", 100, 1), "$Time$.m4s");
    }

    #[test]
    fn expand_unterminated_dollar_preserved() {
        assert_eq!(expand_template("seg$", "id", 100, 1), "seg$");
    }
}
