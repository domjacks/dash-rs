use std::{sync::Arc, time::Instant};

use dash_mpd::{AdaptationSet, Period, SegmentTemplate, MPD};
use url::Url;

use crate::{
    abr::{AbrController, AbrMetrics, QualityDecision, RepresentationInfo},
    error::DashError,
    fetch::{FetchRequest, RequestType, SegmentFetcher},
    manifest::ManifestLoader,
    media_source::{
        sink::SourceBufferSink,
        Chunk, MediaSource,
    },
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
                let sink = SourceBufferSink::new(sink_buf);
                let (start_number, seg_dur_ms, total_segments) =
                    extract_segment_timing(adaptation, period, presentation_duration);
                Some(TrackState::new(MediaType::Video, reps, sink, start_number, seg_dur_ms, total_segments))
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
                let sink = SourceBufferSink::new(sink_buf);
                let (start_number, seg_dur_ms, total_segments) =
                    extract_segment_timing(adaptation, period, presentation_duration);
                Some(TrackState::new(MediaType::Audio, reps, sink, start_number, seg_dur_ms, total_segments))
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
        }
    }

    // ── Buffer scheduling ─────────────────────────────────────────────────────

    /// Record that the pipeline has entered Playing state.
    ///
    /// Call this immediately after your media source transitions to Playing so
    /// that [`Self::estimated_buffer_level_ms`] can subtract elapsed playback
    /// time from the pushed content duration — mirroring the way dash.js reads
    /// `dashMetrics.getCurrentBufferLevel()` to decide whether to schedule the
    /// next fetch.
    pub fn notify_playing(&mut self) {
        self.playback_started_at = Some(Instant::now());
    }

    /// Estimated buffer level ahead of the current playback position, in
    /// milliseconds.
    ///
    /// Computed as `segments_fetched × segment_duration_ms − elapsed_playing_ms`.
    /// This is a wall-clock approximation: it assumes the pipeline plays at
    /// exactly real-time speed, which is true for a non-live pipeline with
    /// correct timestamps.
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
            .map(|t| t.elapsed().as_millis() as f64)
            .unwrap_or(0.0);
        (pushed_ms - elapsed_ms).max(0.0)
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

        // Build the segment URL.
        let (segment_url, seg_number) = {
            let track = self.track(&media_type)?;
            let rep = &track.representations[track.current_index];
            let seg_num = track.segment_number;
            let url = self.resolve_segment_url(&media_type, rep, seg_num)?;
            (url, seg_num)
        };

        tracing::debug!(
            media_type = %media_type,
            segment_number = seg_number,
            url = %segment_url,
            "fetching segment"
        );

        // Fetch.
        let request = FetchRequest::new(segment_url.clone(), RequestType::MediaSegment);
        let response = self.fetcher.fetch(request).await?;

        // Record throughput measurement.
        if let Some(bps) = response.throughput_bps() {
            tracing::debug!(throughput_bps = bps, "measured throughput");
        }

        // Build chunk and append.
        let chunk = Chunk::new(
            response.data,
            None, // Real implementations would parse the MP4 sidx/moov for timing.
            None,
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
    pub async fn fetch_init_segment(
        &mut self,
        media_type: MediaType,
    ) -> Result<usize, DashError> {
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
        let chunk = Chunk::new(
            response.data,
            None,
            None,
            format!("{}/init", media_type),
        );

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

    /// Resolve the URL for the next segment of the given representation.
    ///
    /// Handles `SegmentTemplate` with `$Number$` / `$Number%0Nd$` addressing,
    /// respecting `BaseURL` elements at Period and AdaptationSet level.
    /// `$Time$`, `SegmentList`, and `SegmentBase` are not yet implemented.
    fn resolve_segment_url(
        &self,
        media_type: &MediaType,
        rep: &RepresentationInfo,
        segment_number: u64,
    ) -> Result<Url, DashError> {
        let period = &self.mpd.periods[0];
        let adaptation = period
            .adaptations
            .iter()
            .find(|a| match media_type {
                MediaType::Video => dash_mpd::is_video_adaptation(a),
                MediaType::Audio => dash_mpd::is_audio_adaptation(a),
            })
            .ok_or_else(|| DashError::NoRepresentations {
                media_type: media_type.to_string(),
            })?;

        // Walk SegmentTemplate from AdaptationSet → Period.
        let template = adaptation
            .SegmentTemplate
            .as_ref()
            .or(period.SegmentTemplate.as_ref());

        if let Some(tmpl) = template {
            let media_tmpl = tmpl
                .media
                .as_deref()
                .ok_or(DashError::SegmentUrlUnresolvable)?;

            let url_str =
                expand_template(media_tmpl, &rep.id, rep.bandwidth, segment_number);

            // Chain Period → AdaptationSet BaseURL, then resolve template path.
            let base = resolve_adaptation_base_url(&self.base_url, period, adaptation);
            let resolved = base
                .join(&url_str)
                .map_err(|_| DashError::SegmentUrlUnresolvable)?;

            return Ok(resolved);
        }

        Err(DashError::SegmentUrlUnresolvable)
    }

    /// Resolve the URL for the initialisation segment of the given
    /// representation.
    ///
    /// Handles the common `@initialization` attribute form of `SegmentTemplate`
    /// (e.g. `initialization="video/$RepresentationID$/init.mp4"` or a fixed
    /// path like `initialization="1920x1080p25/IS.mp4"`).
    /// Returns [`DashError::SegmentUrlUnresolvable`] when no initialisation
    /// URL can be derived from the manifest.
    fn resolve_init_url(
        &self,
        media_type: &MediaType,
        rep: &RepresentationInfo,
    ) -> Result<Url, DashError> {
        let period = &self.mpd.periods[0];
        let adaptation = period
            .adaptations
            .iter()
            .find(|a| match media_type {
                MediaType::Video => dash_mpd::is_video_adaptation(a),
                MediaType::Audio => dash_mpd::is_audio_adaptation(a),
            })
            .ok_or_else(|| DashError::NoRepresentations {
                media_type: media_type.to_string(),
            })?;

        // Walk SegmentTemplate from AdaptationSet → Period.
        let template = adaptation
            .SegmentTemplate
            .as_ref()
            .or(period.SegmentTemplate.as_ref());

        if let Some(tmpl) = template {
            // @initialization attribute — the most common form.
            if let Some(init_tmpl) = tmpl.initialization.as_deref() {
                // Segment number is irrelevant for init segments; pass 0.
                let url_str = expand_template(init_tmpl, &rep.id, rep.bandwidth, 0);

                let base = resolve_adaptation_base_url(&self.base_url, period, adaptation);
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
    let tmpl: Option<&SegmentTemplate> = adaptation
        .SegmentTemplate
        .as_ref()
        .or(period.SegmentTemplate.as_ref());

    let Some(t) = tmpl else {
        return (1, 2_000.0, None);
    };

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

/// Resolve the effective base URL for an adaptation set by chaining
/// Period → AdaptationSet `BaseURL` elements against the MPD base URL.
///
/// Each element is resolved with `Url::join` so relative values work
/// correctly. Returns the MPD base URL unchanged when neither level
/// provides a `BaseURL`.
fn resolve_adaptation_base_url(
    mpd_base: &Url,
    period: &Period,
    adaptation: &AdaptationSet,
) -> Url {
    let mut base = mpd_base.clone();

    if let Some(period_base) = period.BaseURL.first() {
        if let Ok(u) = base.join(&period_base.base) {
            base = u;
        }
    }

    if let Some(adapt_base) = adaptation.BaseURL.first() {
        if let Ok(u) = base.join(&adapt_base.base) {
            base = u;
        }
    }

    base
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::expand_template;

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
            expand_template("$RepresentationID$/$Number%06d$.m4s", "1920x1080p25", 4741120, 1),
            "1920x1080p25/000001.m4s"
        );
    }

    #[test]
    fn expand_zero_padded_number_width3() {
        assert_eq!(
            expand_template("$Number%03d$.m4s", "id", 999, 7),
            "007.m4s"
        );
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
        assert_eq!(
            expand_template("$Time$.m4s", "id", 100, 1),
            "$Time$.m4s"
        );
    }

    #[test]
    fn expand_unterminated_dollar_preserved() {
        assert_eq!(expand_template("seg$", "id", 100, 1), "seg$");
    }
}
