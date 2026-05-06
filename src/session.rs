use std::sync::Arc;

use dash_mpd::{AdaptationSet, MPD};
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
    /// How many segments have been fetched so far (used to build URLs when
    /// `SegmentTemplate` `$Number$` addressing is used).
    segment_number: u64,
    /// The `SourceBufferSink` wrapping the underlying buffer.
    sink: SourceBufferSink,
}

impl TrackState {
    fn new(
        media_type: MediaType,
        representations: Vec<RepresentationInfo>,
        sink: SourceBufferSink,
    ) -> Self {
        Self {
            _media_type: media_type,
            representations,
            current_index: 0,
            segment_number: 1,
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

        // Extract video and audio adaptation sets.
        // `period.adaptations` is a plain Vec (not Option<Vec>) in dash-mpd.
        let video_adaptation = period
            .adaptations
            .iter()
            .find(|a| dash_mpd::is_video_adaptation(a));

        let audio_adaptation = period
            .adaptations
            .iter()
            .find(|a| dash_mpd::is_audio_adaptation(a));

        let video_track = if let Some(adaptation) = video_adaptation {
            let reps = extract_representations(adaptation);
            if !reps.is_empty() {
                // Pick the MIME type from the first representation.
                let mime = adaptation_mime_type(adaptation);
                let sink_buf = media_source.add_source_buffer(&mime)?;
                let sink = SourceBufferSink::new(sink_buf);
                Some(TrackState::new(MediaType::Video, reps, sink))
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
                Some(TrackState::new(MediaType::Audio, reps, sink))
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
        })
    }

    // ── Introspection ─────────────────────────────────────────────────────────

    /// The parsed MPD manifest.
    pub fn mpd(&self) -> &MPD {
        &self.mpd
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

    // ── Quality selection ─────────────────────────────────────────────────────

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
    /// Returns the [`QualityDecision`] that was applied.
    pub async fn fetch_next_segment(
        &mut self,
        media_type: MediaType,
        metrics: &AbrMetrics,
    ) -> Result<QualityDecision, DashError> {
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
    /// This is a simplified implementation that handles the common
    /// `SegmentTemplate` with `$Number$` addressing. A production
    /// implementation would also handle `$Time$`, `SegmentList`, and
    /// `SegmentBase` (indexed range requests).
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

        // Walk SegmentTemplate from Representation → AdaptationSet → Period.
        let template = adaptation
            .SegmentTemplate
            .as_ref()
            .or_else(|| period.SegmentTemplate.as_ref());

        if let Some(tmpl) = template {
            let media_tmpl = tmpl
                .media
                .as_deref()
                .ok_or(DashError::SegmentUrlUnresolvable)?;

            let url_str = media_tmpl
                .replace("$RepresentationID$", &rep.id)
                .replace("$Number$", &segment_number.to_string())
                .replace("$Bandwidth$", &rep.bandwidth.to_string());

            // Resolve relative to base URL.
            let resolved = self
                .base_url
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
    /// (e.g. `initialization="video/$RepresentationID$/init.mp4"`).
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

        // Walk SegmentTemplate from AdaptationSet → Period (same precedence
        // as resolve_segment_url).
        let template = adaptation
            .SegmentTemplate
            .as_ref()
            .or_else(|| period.SegmentTemplate.as_ref());

        if let Some(tmpl) = template {
            // @initialization attribute — the most common form.
            if let Some(init_tmpl) = tmpl.initialization.as_deref() {
                let url_str = init_tmpl
                    .replace("$RepresentationID$", &rep.id)
                    .replace("$Bandwidth$", &rep.bandwidth.to_string());

                return self
                    .base_url
                    .join(&url_str)
                    .map_err(|_| DashError::SegmentUrlUnresolvable);
            }
        }

        Err(DashError::SegmentUrlUnresolvable)
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// Extract [`RepresentationInfo`] from an [`AdaptationSet`], sorted by
/// bandwidth ascending.
fn extract_representations(adaptation: &AdaptationSet) -> Vec<RepresentationInfo> {
    let mut reps: Vec<RepresentationInfo> = adaptation
        .representations
        .iter()
        .filter_map(|r| {
            let bandwidth = r.bandwidth?;
            Some(RepresentationInfo {
                id: r.id.clone().unwrap_or_default(),
                bandwidth: bandwidth as u64,
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
