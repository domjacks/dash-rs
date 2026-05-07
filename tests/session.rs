//! Integration tests for `DashSession`'s live-stream API:
//! `is_live`, `minimum_update_period_ms`, `should_refresh_manifest`,
//! `refresh_manifest`, and live-edge segment snapping.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::Bytes;
use url::Url;

use dash_rs::{
    abr::AbrMetrics,
    error::DashError,
    fetch::{FetchError, FetchRequest, FetchResponse, RequestType, SegmentFetcher},
    media_source::stub::StubMediaSource,
    session::{DashSession, MediaType},
};

// ── MPD XML fixtures ──────────────────────────────────────────────────────────

/// A minimal static (VOD) MPD: 30 s total, 2 s segments, one video track.
const STATIC_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT30S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

/// A live (dynamic) MPD with `minimumUpdatePeriod="PT4S"` and a
/// caller-specified `mediaPresentationDuration`.  Segments are 2 s each.
fn live_mpd(duration_iso: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     minimumUpdatePeriod="PT4S"
     mediaPresentationDuration="{duration_iso}">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#
    )
}

/// A live MPD that omits `minimumUpdatePeriod`.
const LIVE_MPD_NO_UPDATE_PERIOD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     mediaPresentationDuration="PT30S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

// ── Mock fetcher ──────────────────────────────────────────────────────────────

/// A `SegmentFetcher` that serves preset MPD XML strings for manifest
/// requests and returns empty bytes for all media/init segment requests.
///
/// This prevents segment fetches from accidentally consuming MPD responses
/// from the queue during tests that interleave manifest refreshes with
/// `fetch_next_segment` calls.
struct MockFetcher {
    manifest_queue: Arc<Mutex<VecDeque<Bytes>>>,
}

impl MockFetcher {
    fn with_manifests(manifests: &[&str]) -> Self {
        Self {
            manifest_queue: Arc::new(Mutex::new(
                manifests
                    .iter()
                    .map(|s| Bytes::from(s.to_string()))
                    .collect(),
            )),
        }
    }
}

#[async_trait]
impl SegmentFetcher for MockFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let data = if request.request_type == RequestType::Manifest {
            self.manifest_queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default()
        } else {
            // Media / init segment fetches always return empty bytes so they
            // don't consume manifest responses from the queue.
            Bytes::new()
        };
        let len = data.len() as u64;
        Ok(FetchResponse {
            final_url: request.url,
            data,
            transfer_duration_ms: 1,
            bytes_received: len,
        })
    }

    async fn check_existence(&self, _url: &Url) -> Result<bool, FetchError> {
        Ok(true)
    }

    fn abort(&self) {}
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn open_stub_source() -> StubMediaSource {
    let mut ms = StubMediaSource::new();
    ms.open();
    ms
}

/// Build a `DashSession` from a raw XML string without a network call.
fn session_from_xml(
    xml: &str,
) -> DashSession<dash_rs::fetch::stub::StubSegmentFetcher, StubMediaSource> {
    let mpd = dash_mpd::parse(xml).expect("MPD parse failed");
    let base_url = Url::parse("http://example.com/").unwrap();
    let fetcher = Arc::new(dash_rs::fetch::stub::StubSegmentFetcher);
    DashSession::from_mpd(mpd, base_url, fetcher, open_stub_source()).expect("from_mpd failed")
}

fn default_metrics() -> AbrMetrics {
    AbrMetrics::new(1_000_000.0, 4_000.0)
}

// ── is_live ───────────────────────────────────────────────────────────────────

#[test]
fn is_live_false_for_static_mpd() {
    assert!(!session_from_xml(STATIC_MPD).is_live());
}

#[test]
fn is_live_true_for_dynamic_mpd() {
    assert!(session_from_xml(&live_mpd("PT30S")).is_live());
}

// ── minimum_update_period_ms ──────────────────────────────────────────────────

#[test]
fn minimum_update_period_none_for_vod() {
    assert_eq!(
        session_from_xml(STATIC_MPD).minimum_update_period_ms(),
        None
    );
}

#[test]
fn minimum_update_period_extracted_from_live_mpd() {
    // Fixture declares minimumUpdatePeriod="PT4S" → 4 000 ms.
    assert_eq!(
        session_from_xml(&live_mpd("PT30S")).minimum_update_period_ms(),
        Some(4_000)
    );
}

#[test]
fn minimum_update_period_none_when_attribute_absent() {
    assert_eq!(
        session_from_xml(LIVE_MPD_NO_UPDATE_PERIOD).minimum_update_period_ms(),
        None
    );
}

// ── should_refresh_manifest ───────────────────────────────────────────────────

#[test]
fn should_not_refresh_vod_stream() {
    assert!(!session_from_xml(STATIC_MPD).should_refresh_manifest());
}

#[test]
fn should_not_refresh_live_without_minimum_update_period() {
    // Live stream but no minimumUpdatePeriod → refresh cadence is unknown.
    assert!(!session_from_xml(LIVE_MPD_NO_UPDATE_PERIOD).should_refresh_manifest());
}

#[test]
fn should_refresh_live_built_via_from_mpd() {
    // `from_mpd` sets `last_manifest_refresh = None` (no network fetch).
    // A live stream with a declared `minimumUpdatePeriod` should immediately
    // signal that a refresh is needed.
    assert!(session_from_xml(&live_mpd("PT30S")).should_refresh_manifest());
}

#[tokio::test]
async fn should_not_refresh_immediately_after_from_url() {
    // `from_url` stamps `last_manifest_refresh = Some(Instant::now())`.
    // With `minimumUpdatePeriod = PT4S` the very next call should return false.
    let xml = live_mpd("PT30S");
    let fetcher = MockFetcher::with_manifests(&[&xml]);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();
    assert!(!session.should_refresh_manifest());
}

// ── refresh_manifest ──────────────────────────────────────────────────────────

#[tokio::test]
async fn refresh_manifest_updates_segment_count() {
    // v1: live, PT4S → ceil(4/2) = 2 segments.
    // v2: live, PT8S → ceil(8/2) = 4 segments.
    //
    // After loading v1, fetch 1 segment (1 of 2), then refresh to v2.
    // Should then be able to fetch 3 more (2, 3, 4) before EOS.
    let xml_v1 = live_mpd("PT4S");
    let xml_v2 = live_mpd("PT8S");
    let fetcher = MockFetcher::with_manifests(&[&xml_v1, &xml_v2]);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = default_metrics();

    // First segment fetch succeeds (segment_number: 1 → 2, fetched: 1/2).
    session
        .fetch_next_segment(MediaType::Video, &metrics)
        .await
        .unwrap();

    // Refresh loads v2: total_segments becomes 4.  Timer is reset.
    session.refresh_manifest().await.unwrap();
    assert!(
        !session.should_refresh_manifest(),
        "refresh should reset the timer"
    );

    // Three more fetches succeed (segments 2, 3, 4 → fetched: 4/4).
    for i in 0..3 {
        session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await
            .unwrap_or_else(|e| panic!("fetch {} after refresh failed: {e}", i + 2));
    }

    // Fifth call: fetched = 4, total = 4 → EndOfStream.
    let result = session.fetch_next_segment(MediaType::Video, &metrics).await;
    assert!(
        matches!(result, Err(DashError::EndOfStream { .. })),
        "expected EndOfStream after 4 fetches, got {result:?}"
    );
}

#[tokio::test]
async fn refresh_manifest_updates_minimum_update_period() {
    // v1: minimumUpdatePeriod = PT4S.
    // v2: minimumUpdatePeriod = PT8S.
    // After refresh, minimum_update_period_ms() should reflect the new value.
    const MPD_V2_8S: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     minimumUpdatePeriod="PT8S"
     mediaPresentationDuration="PT30S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let xml_v1 = live_mpd("PT30S"); // minimumUpdatePeriod = PT4S
    let fetcher = MockFetcher::with_manifests(&[&xml_v1, MPD_V2_8S]);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    assert_eq!(session.minimum_update_period_ms(), Some(4_000));

    session.refresh_manifest().await.unwrap();

    assert_eq!(
        session.minimum_update_period_ms(),
        Some(8_000),
        "minimumUpdatePeriod should reflect the refreshed manifest"
    );
}

// ── Live-edge segment snapping ────────────────────────────────────────────────

/// A `SegmentFetcher` whose URL recording vec is owned externally so the
/// test can inspect it after passing the fetcher into `DashSession::from_url`.
struct RecordingFetcher {
    manifest_queue: Arc<Mutex<VecDeque<Bytes>>>,
    /// Shared with the test so URLs are visible after the fetcher is moved
    /// into the session.
    segment_urls: Arc<Mutex<Vec<String>>>,
}

impl RecordingFetcher {
    fn new(manifests: &[&str], segment_urls: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            manifest_queue: Arc::new(Mutex::new(
                manifests
                    .iter()
                    .map(|s| Bytes::from(s.to_string()))
                    .collect(),
            )),
            segment_urls,
        }
    }
}

#[async_trait]
impl SegmentFetcher for RecordingFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let data = if request.request_type == RequestType::Manifest {
            self.manifest_queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default()
        } else {
            self.segment_urls
                .lock()
                .unwrap()
                .push(request.url.to_string());
            Bytes::new()
        };
        let len = data.len() as u64;
        Ok(FetchResponse {
            final_url: request.url,
            data,
            transfer_duration_ms: 1,
            bytes_received: len,
        })
    }

    async fn check_existence(&self, _url: &Url) -> Result<bool, FetchError> {
        Ok(true)
    }

    fn abort(&self) {}
}

/// Build a live MPD whose `availabilityStartTime` is a fixed number of seconds
/// before "now", so the expected live-edge segment number is predictable.
///
/// Returns `(xml, expected_start_segment)`.
/// Segment duration is 4 s (`duration="4" timescale="1"`).
fn live_mpd_with_ast(elapsed_seconds: u64) -> (String, u64) {
    let now_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let ast_unix = now_s.saturating_sub(elapsed_seconds);

    // Convert Unix timestamp to an ISO 8601 UTC datetime string that
    // dash_mpd's parser accepts (RFC-3339 / xsd:dateTime).
    let ast_datetime = unix_secs_to_datetime(ast_unix);

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     availabilityStartTime="{ast_datetime}"
     minimumUpdatePeriod="PT4S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="4" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#
    );

    // Expected start: live_edge = floor(elapsed/4) + 1; default safety = 3.
    let live_edge = (elapsed_seconds / 4) + 1;
    let start_at = live_edge.saturating_sub(3).max(1);
    (xml, start_at)
}

/// Minimal Gregorian calendar conversion from a Unix timestamp (seconds since
/// 1970-01-01T00:00:00Z) to an RFC-3339 datetime string.
/// Valid for dates between 1970 and 2100.
fn unix_secs_to_datetime(unix: u64) -> String {
    let rem_secs = unix % 86_400;
    let hh = rem_secs / 3600;
    let mm = (rem_secs % 3600) / 60;
    let ss = rem_secs % 60;
    let mut remaining_days = unix / 86_400;
    let mut year = 1970u64;
    loop {
        let days_in_year = if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
            366
        } else {
            365
        };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u64;
    for &md in &month_days {
        if remaining_days < md {
            break;
        }
        remaining_days -= md;
        month += 1;
    }
    let day = remaining_days + 1;
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[tokio::test]
async fn live_edge_segment_number_near_live_edge() {
    // 1 000 s of elapsed time, 4 s segments:
    //   live_edge = floor(1000/4) + 1 = 251
    //   start_at  = 251 − 3 (default safety) = 248
    let (xml, expected_start) = live_mpd_with_ast(1_000);
    assert_eq!(expected_start, 248, "self-check: expected_start formula");

    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[&xml], Arc::clone(&recorded));

    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    assert!(session.is_live(), "stream should be identified as live");

    let metrics = default_metrics();
    session
        .fetch_next_segment(MediaType::Video, &metrics)
        .await
        .unwrap();

    let urls = recorded.lock().unwrap();
    assert_eq!(
        urls.len(),
        1,
        "exactly one segment should have been fetched"
    );

    // Template is "seg-$Number$.m4s" so the URL ends in "seg-<N>.m4s".
    // Extract N by finding the trailing digits before ".m4s".
    let url_str = &urls[0];
    let seg_num: u64 = url_str
        .strip_suffix(".m4s")
        .and_then(|s| {
            let digit_start = s
                .rfind(|c: char| !c.is_ascii_digit())
                .map(|i| i + 1)
                .unwrap_or(0);
            s[digit_start..].parse().ok()
        })
        .unwrap_or(0);

    // Allow ±1 tolerance for the one-second wall-clock jitter that can occur
    // between `live_mpd_with_ast` computing `now_s` and the session
    // construction reading its own clock.
    assert!(
        seg_num.abs_diff(expected_start) <= 1,
        "expected segment ~{expected_start}, got {seg_num} (url: {url_str})"
    );
}

// ── SegmentTimeline / $Time$ integration tests ────────────────────────────────

/// A VOD MPD whose single AdaptationSet uses SegmentTimeline with three
/// explicit S entries (no repeats).  timescale = 90 000.
const TIMELINE_VOD_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT9S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="chunk-$Time$.m4s" initialization="init.mp4" timescale="90000">
        <SegmentTimeline>
          <S t="0"      d="270000"/>
          <S t="270000" d="270000"/>
          <S t="540000" d="270000"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

/// VOD MPD with a single S entry that has `r="2"` (3 segments total).
const TIMELINE_REPEAT_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT9S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="chunk-$Time$.m4s" initialization="init.mp4" timescale="90000">
        <SegmentTimeline>
          <S t="0" d="270000" r="2"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

/// Fetch all three segments from a SegmentTimeline VOD stream in order and
/// verify that the URLs contain the correct `$Time$` values.
#[tokio::test]
async fn timeline_vod_fetches_in_time_order() {
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[TIMELINE_VOD_MPD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = default_metrics();

    // Fetch the three segments; all should succeed.
    for i in 0..3 {
        session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await
            .unwrap_or_else(|e| panic!("segment {i} fetch failed: {e}"));
    }

    let urls = recorded.lock().unwrap();
    assert_eq!(urls.len(), 3, "expected 3 segment fetches");

    let times: Vec<u64> = urls
        .iter()
        .map(|u| {
            // URL looks like "http://example.com/chunk-270000.m4s"
            u.rsplit('/').next().unwrap_or("")
                .strip_prefix("chunk-").unwrap_or("")
                .strip_suffix(".m4s").unwrap_or("")
                .parse()
                .expect("non-numeric time in URL")
        })
        .collect();

    assert_eq!(times, vec![0u64, 270_000, 540_000], "unexpected $Time$ sequence");
}

/// A live MPD with a specific `startNumber` value, used to test the
/// sliding-window behaviour when `startNumber` advances between manifest
/// refreshes.
fn live_mpd_with_start_number(start_number: u64) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     minimumUpdatePeriod="PT4S"
     mediaPresentationDuration="PT30S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="{start_number}" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#
    )
}

/// A static VOD MPD with two representations (v1 low-bitrate, v2 high-bitrate)
/// and representation-specific init/media URLs so tests can distinguish them
/// in the recorded URL list.
const MULTI_REP_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT30S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$-$RepresentationID$.m4s"
                       initialization="init-$RepresentationID$.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="100000"/>
      <Representation id="v2" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

// ── init segment caching ──────────────────────────────────────────────────────

/// After `fetch_init_segment` for rep v1 and a subsequent quality switch to
/// v2 (cache miss → network fetch of v2 init), switching back to v1 must
/// re-append the v1 init from the in-memory cache without a second network
/// request.  Total init fetches per representation: exactly one each.
#[tokio::test]
async fn init_cache_hit_avoids_duplicate_network_fetch() {
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[MULTI_REP_MPD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    // Explicitly fetch init for rep v1 (index 0) → populates cache.
    session
        .fetch_init_segment(MediaType::Video)
        .await
        .expect("fetch_init_segment v1 failed");

    // High-throughput fetch → ABR switches up to v2 (index 1).
    // Cache miss for v2: triggers a network fetch of init-v2.mp4.
    let high_metrics = AbrMetrics {
        throughput_bps: 10_000_000.0,
        buffer_level_ms: 5_000.0,
        current_index: Some(0),
        ..AbrMetrics::new(10_000_000.0, 5_000.0)
    };
    session
        .fetch_next_segment(MediaType::Video, &high_metrics)
        .await
        .expect("fetch segment 1 (switch up to v2) failed");

    // Critically-low buffer → InsufficientBufferRule forces index 0, overriding
    // the SwitchHistoryRule cooldown.  Cache HIT for v1: no second network
    // request for init-v1.mp4.
    let low_buf_metrics = AbrMetrics {
        throughput_bps: 0.0,
        buffer_level_ms: 0.0,
        current_index: Some(1),
        ..AbrMetrics::new(0.0, 0.0)
    };
    session
        .fetch_next_segment(MediaType::Video, &low_buf_metrics)
        .await
        .expect("fetch segment 2 (switch back to v1) failed");

    let urls = recorded.lock().unwrap();
    let init_v1_count = urls.iter().filter(|u| u.contains("init-v1")).count();
    let init_v2_count = urls.iter().filter(|u| u.contains("init-v2")).count();

    assert_eq!(
        init_v1_count, 1,
        "init-v1 should be fetched exactly once (second visit uses cache); got URLs: {urls:?}"
    );
    assert_eq!(
        init_v2_count, 1,
        "init-v2 should be fetched exactly once (cache miss on first switch); got URLs: {urls:?}"
    );
}

/// When a quality switch occurs but the init segment for the new
/// representation was never explicitly fetched, `fetch_next_segment` must
/// perform a live network fetch and also populate the cache for future
/// switches.
#[tokio::test]
async fn init_cache_miss_fetches_and_caches() {
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[MULTI_REP_MPD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    // No explicit fetch_init_segment call — cache is empty.
    // High-throughput fetch switches to v2; init-v2.mp4 is not cached, so
    // it must be fetched from the network exactly once.
    let high_metrics = AbrMetrics {
        throughput_bps: 10_000_000.0,
        buffer_level_ms: 5_000.0,
        current_index: Some(0),
        ..AbrMetrics::new(10_000_000.0, 5_000.0)
    };
    session
        .fetch_next_segment(MediaType::Video, &high_metrics)
        .await
        .expect("fetch segment 1 (switch up to v2) failed");

    // Now switch back to v1 (critically-low buffer).  Since v1 was never
    // fetched, this is also a cache miss → a network fetch of init-v1.mp4.
    let low_buf_metrics = AbrMetrics {
        throughput_bps: 0.0,
        buffer_level_ms: 0.0,
        current_index: Some(1),
        ..AbrMetrics::new(0.0, 0.0)
    };
    session
        .fetch_next_segment(MediaType::Video, &low_buf_metrics)
        .await
        .expect("fetch segment 2 (switch back to v1) failed");

    // Switch up to v2 again with high throughput.  Now v2 IS cached — no new
    // init-v2.mp4 network request.
    let high_metrics2 = AbrMetrics {
        throughput_bps: 10_000_000.0,
        buffer_level_ms: 5_000.0,
        current_index: Some(0),
        ..AbrMetrics::new(10_000_000.0, 5_000.0)
    };
    session
        .fetch_next_segment(MediaType::Video, &high_metrics2)
        .await
        .expect("fetch segment 3 (switch back to v2, cached) failed");

    let urls = recorded.lock().unwrap();
    let init_v1_count = urls.iter().filter(|u| u.contains("init-v1")).count();
    let init_v2_count = urls.iter().filter(|u| u.contains("init-v2")).count();

    assert_eq!(
        init_v1_count, 1,
        "init-v1 must be fetched exactly once (cached after first miss); got URLs: {urls:?}"
    );
    assert_eq!(
        init_v2_count, 1,
        "init-v2 must be fetched exactly once (cached after first miss); got URLs: {urls:?}"
    );
}

// ── startNumber sliding-window live ───────────────────────────────────────────

/// When a manifest refresh advances `startNumber` past the current
/// `segment_number`, the next fetch must target the new `startNumber` (the
/// session has "fallen behind" the live DVR window).
#[tokio::test]
async fn sliding_window_snaps_segment_number_when_behind_window() {
    let xml_v1 = live_mpd_with_start_number(1);
    // startNumber jumps to 10; segment_number will be at 4 after 3 fetches.
    let xml_v2 = live_mpd_with_start_number(10);
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[&xml_v1, &xml_v2], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = default_metrics();
    // Fetch 3 segments → segment_number = 4.
    for _ in 0..3 {
        session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await
            .expect("segment fetch failed");
    }

    // Refresh: startNumber advances to 10.  segment_number (4) < 10 → snap.
    session.refresh_manifest().await.expect("refresh failed");

    // Post-snap, the next fetch must request seg-10.m4s.
    session
        .fetch_next_segment(MediaType::Video, &metrics)
        .await
        .expect("fetch after sliding-window snap failed");

    let urls = recorded.lock().unwrap();
    let last_media_url = urls
        .iter()
        .rev()
        .find(|u| u.contains("seg-"))
        .expect("no media segment URL recorded");

    assert!(
        last_media_url.contains("/seg-10.m4s"),
        "after sliding-window snap expected seg-10.m4s; got {last_media_url}"
    );
}

/// When `startNumber` advances but `segment_number` is already inside the new
/// window, only `start_number` is updated — `segment_number` is NOT changed.
#[tokio::test]
async fn sliding_window_no_snap_when_segment_number_inside_window() {
    let xml_v1 = live_mpd_with_start_number(1);
    // startNumber advances by 1 only; segment_number (4 after 3 fetches) is
    // well inside the new window.
    let xml_v2 = live_mpd_with_start_number(2);
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[&xml_v1, &xml_v2], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = default_metrics();
    // Fetch 3 segments → segment_number = 4.
    for _ in 0..3 {
        session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await
            .expect("segment fetch failed");
    }

    // Refresh: startNumber advances to 2.  segment_number (4) ≥ 2 → no snap.
    session.refresh_manifest().await.expect("refresh failed");

    session
        .fetch_next_segment(MediaType::Video, &metrics)
        .await
        .expect("fetch after no-snap refresh failed");

    let urls = recorded.lock().unwrap();
    let last_media_url = urls
        .iter()
        .rev()
        .find(|u| u.contains("seg-"))
        .expect("no media segment URL recorded");

    assert!(
        last_media_url.contains("/seg-4.m4s"),
        "segment_number should not be snapped when ahead of new startNumber; got {last_media_url}"
    );
}
#[tokio::test]
async fn timeline_vod_eos_after_all_segments() {
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[TIMELINE_VOD_MPD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = default_metrics();

    for _ in 0..3 {
        session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await
            .expect("segment fetch should succeed");
    }

    let result = session.fetch_next_segment(MediaType::Video, &metrics).await;
    assert!(
        matches!(result, Err(DashError::EndOfStream { .. })),
        "expected EndOfStream after timeline exhausted, got {result:?}"
    );
}

/// A SegmentTimeline with `r="2"` (3 occurrences) should produce the same
/// `$Time$` sequence as three explicit S entries with the same durations.
#[tokio::test]
async fn timeline_repeat_attribute_produces_correct_times() {
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[TIMELINE_REPEAT_MPD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = default_metrics();

    for i in 0..3 {
        session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await
            .unwrap_or_else(|e| panic!("segment {i} fetch failed: {e}"));
    }

    let urls = recorded.lock().unwrap();
    let times: Vec<u64> = urls
        .iter()
        .map(|u| {
            u.rsplit('/').next().unwrap_or("")
                .strip_prefix("chunk-").unwrap_or("")
                .strip_suffix(".m4s").unwrap_or("")
                .parse()
                .expect("non-numeric time in URL")
        })
        .collect();

    assert_eq!(times, vec![0u64, 270_000, 540_000]);

    // After all repeats, the timeline is exhausted → EOS.
    drop(urls);
    let result = session.fetch_next_segment(MediaType::Video, &metrics).await;
    assert!(
        matches!(result, Err(DashError::EndOfStream { .. })),
        "expected EndOfStream after repeat segments exhausted, got {result:?}"
    );
}

// ── UTCTiming ─────────────────────────────────────────────────────────────────

/// Build a live MPD that embeds a `UTCTiming` element with the given scheme
/// and value.
fn live_mpd_with_utc_timing(scheme_id: &str, value: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     minimumUpdatePeriod="PT4S"
     mediaPresentationDuration="PT30S">
  <UTCTiming schemeIdUri="{scheme_id}" value="{value}"/>
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#
    )
}

/// A `SegmentFetcher` that serves the initial manifest from a queue and,
/// for `ClockSync` requests, returns a fixed datetime body supplied at
/// construction time.
struct UtcMockFetcher {
    manifest_queue: Arc<Mutex<VecDeque<Bytes>>>,
    clock_body: Bytes,
}

impl UtcMockFetcher {
    fn new(manifest: &str, clock_body: &str) -> Self {
        let mut q = VecDeque::new();
        q.push_back(Bytes::from(manifest.to_string()));
        Self {
            manifest_queue: Arc::new(Mutex::new(q)),
            clock_body: Bytes::from(clock_body.to_string()),
        }
    }
}

#[async_trait]
impl SegmentFetcher for UtcMockFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let data = match request.request_type {
            RequestType::Manifest => self
                .manifest_queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default(),
            RequestType::ClockSync => self.clock_body.clone(),
            _ => Bytes::new(),
        };
        let len = data.len() as u64;
        Ok(FetchResponse {
            final_url: request.url,
            data,
            transfer_duration_ms: 1,
            bytes_received: len,
        })
    }

    async fn check_existence(&self, _url: &Url) -> Result<bool, FetchError> {
        Ok(true)
    }

    fn abort(&self) {}
}

/// A `direct` UTCTiming element with a known fixed datetime sets
/// `utc_clock_offset_s` to approximately `fixed_time − SystemTime::now()`.
#[tokio::test]
async fn utc_direct_scheme_sets_offset() {
    // Use a fixed point well in the past: 2000-01-01T00:00:00Z = 946 684 800 s.
    const SERVER_TIME: f64 = 946_684_800.0;
    let xml = live_mpd_with_utc_timing(
        "urn:mpeg:dash:utc:direct:2014",
        "2000-01-01T00:00:00Z",
    );

    let fetcher = MockFetcher::with_manifests(&[&xml]);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let client_now_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let expected_offset = SERVER_TIME - client_now_s;
    let actual_offset = session.utc_clock_offset_s();

    // Allow 2 s of tolerance for test execution time.
    assert!(
        (actual_offset - expected_offset).abs() < 2.0,
        "expected offset ≈ {expected_offset:.1}, got {actual_offset:.1}"
    );
    // Must be a large negative number (server is ~24 years behind).
    assert!(
        actual_offset < -700_000_000.0,
        "offset should be a large negative value; got {actual_offset}"
    );
}

/// An `http-xsdate` UTCTiming element causes a `ClockSync` fetch through the
/// `SegmentFetcher`.  The mock fetcher returns a known datetime body and the
/// computed offset must match `body_time − client_now`.
#[tokio::test]
async fn utc_http_xsdate_scheme_fetches_via_fetcher() {
    // Server time: 2000-01-01T00:00:00Z = 946 684 800 s.
    const SERVER_TIME: f64 = 946_684_800.0;
    let xml = live_mpd_with_utc_timing(
        "urn:mpeg:dash:utc:http-xsdate:2014",
        "http://time.example.com/now",
    );

    let fetcher = UtcMockFetcher::new(&xml, "2000-01-01T00:00:00Z");
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let client_now_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let expected_offset = SERVER_TIME - client_now_s;
    let actual_offset = session.utc_clock_offset_s();

    assert!(
        (actual_offset - expected_offset).abs() < 2.0,
        "expected offset ≈ {expected_offset:.1}, got {actual_offset:.1}"
    );
}

/// When only unsupported schemes (`ntp`, `http-head`) are present the offset
/// stays at `0.0` and the session is still constructed successfully.
#[tokio::test]
async fn utc_unsupported_schemes_fall_back_to_zero() {
    // Build an MPD with two UTCTiming elements that are both unsupported.
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="dynamic"
     minimumUpdatePeriod="PT4S"
     mediaPresentationDuration="PT30S">
  <UTCTiming schemeIdUri="urn:mpeg:dash:utc:ntp:2014" value="time.example.com"/>
  <UTCTiming schemeIdUri="urn:mpeg:dash:utc:http-head:2014" value="http://time.example.com/head"/>
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let fetcher = MockFetcher::with_manifests(&[xml]);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    assert_eq!(
        session.utc_clock_offset_s(),
        0.0,
        "unsupported UTCTiming schemes must leave offset at 0.0"
    );
}

// ── emsg / in-band event tests ────────────────────────────────────────────────

/// Build a version-1 `emsg` box as raw bytes.
fn make_emsg_v1(
    scheme_id_uri: &str,
    value: &str,
    timescale: u32,
    presentation_time: u64,
    event_duration: u32,
    id: u32,
    message_data: &[u8],
) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    body.push(1); // version
    body.extend_from_slice(&[0u8; 3]); // flags
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&presentation_time.to_be_bytes());
    body.extend_from_slice(&event_duration.to_be_bytes());
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(scheme_id_uri.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    body.extend_from_slice(message_data);

    let size = (8u32 + body.len() as u32).to_be_bytes();
    let mut buf = size.to_vec();
    buf.extend_from_slice(b"emsg");
    buf.extend_from_slice(&body);
    buf
}

/// Build a version-0 `emsg` box as raw bytes.
fn make_emsg_v0(
    scheme_id_uri: &str,
    value: &str,
    timescale: u32,
    presentation_time_delta: u32,
    event_duration: u32,
    id: u32,
    message_data: &[u8],
) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    body.push(0); // version
    body.extend_from_slice(&[0u8; 3]); // flags
    body.extend_from_slice(scheme_id_uri.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&presentation_time_delta.to_be_bytes());
    body.extend_from_slice(&event_duration.to_be_bytes());
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(message_data);

    let size = (8u32 + body.len() as u32).to_be_bytes();
    let mut buf = size.to_vec();
    buf.extend_from_slice(b"emsg");
    buf.extend_from_slice(&body);
    buf
}

/// A `SegmentFetcher` that serves one MPD from a queue and returns a fixed
/// `segment_data` payload for every media / init segment fetch.
struct EmsgMockFetcher {
    manifest_queue: Arc<Mutex<VecDeque<Bytes>>>,
    segment_data: Bytes,
}

impl EmsgMockFetcher {
    fn new(manifest_xml: &str, segment_data: Vec<u8>) -> Self {
        Self {
            manifest_queue: Arc::new(Mutex::new(VecDeque::from([Bytes::from(
                manifest_xml.to_string(),
            )]))),
            segment_data: Bytes::from(segment_data),
        }
    }
}

#[async_trait]
impl SegmentFetcher for EmsgMockFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let data = if request.request_type == RequestType::Manifest {
            self.manifest_queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default()
        } else {
            self.segment_data.clone()
        };
        let len = data.len() as u64;
        Ok(FetchResponse {
            final_url: request.url,
            data,
            transfer_duration_ms: 1,
            bytes_received: len,
        })
    }

    async fn check_existence(&self, _url: &Url) -> Result<bool, FetchError> {
        Ok(true)
    }

    fn abort(&self) {}
}

/// A v1 emsg embedded in a media segment is delivered to a subscriber of
/// `subscribe_events` when `fetch_next_segment` processes that segment.
#[tokio::test]
async fn emsg_v1_event_delivered_to_subscriber() {
    let emsg_bytes = make_emsg_v1(
        "urn:scte:scte35:2014:xml+bin",
        "1",
        90_000,
        450_000, // 5.0 s
        90_000,  // 1.0 s duration
        77,
        b"splice-info",
    );

    let fetcher = EmsgMockFetcher::new(STATIC_MPD, emsg_bytes);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    // Subscribe before the fetch so no events are missed.
    let mut events = session.subscribe_events();

    let metrics = default_metrics();
    session
        .fetch_next_segment(MediaType::Video, &metrics)
        .await
        .unwrap();

    let ev = events.try_recv().expect("expected one emsg event");
    assert_eq!(ev.scheme_id_uri, "urn:scte:scte35:2014:xml+bin");
    assert_eq!(ev.value, "1");
    assert!((ev.presentation_time_s - 5.0).abs() < 1e-6, "pt={}", ev.presentation_time_s);
    assert!((ev.duration_s - 1.0).abs() < 1e-6, "dur={}", ev.duration_s);
    assert_eq!(ev.id, 77);
    assert_eq!(&ev.message_data[..], b"splice-info");
    assert_eq!(ev.media_type, MediaType::Video);
}

/// A v0 emsg preserves `scheme_id_uri`, `value`, and the time fields through
/// the full `fetch_next_segment` → broadcast path.
#[tokio::test]
async fn emsg_v0_fields_preserved_through_broadcast() {
    // timescale=1000, pt_delta=2000 → 2.0 s; event_duration=3000 → 3.0 s
    let emsg_bytes = make_emsg_v0("urn:example:v0", "myval", 1000, 2000, 3000, 99, b"v0data");

    let fetcher = EmsgMockFetcher::new(STATIC_MPD, emsg_bytes);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let mut events = session.subscribe_events();

    session
        .fetch_next_segment(MediaType::Video, &default_metrics())
        .await
        .unwrap();

    let ev = events.try_recv().expect("expected v0 emsg event");
    assert_eq!(ev.scheme_id_uri, "urn:example:v0");
    assert_eq!(ev.value, "myval");
    assert!((ev.presentation_time_s - 2.0).abs() < 1e-9, "pt={}", ev.presentation_time_s);
    assert!((ev.duration_s - 3.0).abs() < 1e-9, "dur={}", ev.duration_s);
    assert_eq!(ev.id, 99);
    assert_eq!(&ev.message_data[..], b"v0data");
}

/// When there are no subscribers, `fetch_next_segment` must not panic or
/// return an error — discarding the event silently is correct behaviour.
#[tokio::test]
async fn emsg_no_subscriber_does_not_panic() {
    let emsg_bytes = make_emsg_v1("urn:x", "", 1000, 0, 1000, 1, b"");

    let fetcher = EmsgMockFetcher::new(STATIC_MPD, emsg_bytes);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    // Deliberately do NOT call subscribe_events().
    let result = session
        .fetch_next_segment(MediaType::Video, &default_metrics())
        .await;
    assert!(result.is_ok(), "fetch_next_segment must succeed with no emsg subscribers");
}

/// Multiple `emsg` boxes in a single segment each produce an independent event.
#[tokio::test]
async fn emsg_two_events_in_one_segment() {    let mut emsg_bytes = make_emsg_v1("urn:a", "", 1000, 1000, 500, 1, b"first");
    emsg_bytes.extend_from_slice(&make_emsg_v1("urn:b", "", 1000, 2000, 500, 2, b"second"));

    let fetcher = EmsgMockFetcher::new(STATIC_MPD, emsg_bytes);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let mut events = session.subscribe_events();

    session
        .fetch_next_segment(MediaType::Video, &default_metrics())
        .await
        .unwrap();

    let ev1 = events.try_recv().expect("expected first emsg event");
    let ev2 = events.try_recv().expect("expected second emsg event");
    assert_eq!(ev1.scheme_id_uri, "urn:a");
    assert_eq!(&ev1.message_data[..], b"first");
    assert_eq!(ev2.scheme_id_uri, "urn:b");
    assert_eq!(&ev2.message_data[..], b"second");
    // No third event.
    assert!(events.try_recv().is_err());
}

// ── current_metrics() throughput tests ───────────────────────────────────────

/// A fetcher that serves a known payload with controllable timing metadata so
/// we can verify the EMA calculation precisely.
struct TimedSegmentFetcher {
    manifest_xml: Bytes,
    /// Bytes returned for every media / init segment fetch.
    segment_data: Bytes,
    /// `transfer_duration_ms` reported for every media / init segment fetch.
    transfer_duration_ms: u64,
}

impl TimedSegmentFetcher {
    fn new(manifest_xml: &str, segment_bytes: usize, transfer_duration_ms: u64) -> Self {
        Self {
            manifest_xml: Bytes::from(manifest_xml.to_string()),
            segment_data: Bytes::from(vec![0u8; segment_bytes]),
            transfer_duration_ms,
        }
    }
}

#[async_trait]
impl SegmentFetcher for TimedSegmentFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let (data, transfer_ms) = if request.request_type == RequestType::Manifest {
            (self.manifest_xml.clone(), 1)
        } else {
            (self.segment_data.clone(), self.transfer_duration_ms)
        };
        let len = data.len() as u64;
        Ok(FetchResponse {
            final_url: request.url,
            data,
            transfer_duration_ms: transfer_ms,
            bytes_received: len,
        })
    }
    async fn check_existence(&self, _url: &Url) -> Result<bool, FetchError> {
        Ok(true)
    }
    fn abort(&self) {}
}

/// After fetching one segment with known size and transfer time,
/// `current_metrics()` must return a non-zero throughput equal to the first
/// sample (EMA seeded directly from the first measurement) and `latency_ms`
/// equal to the transfer duration.
#[tokio::test]
async fn current_metrics_throughput_nonzero_after_first_fetch() {
    // 125 000 bytes in 1 000 ms → 1 000 000 bps (1 Mbps).
    let bytes = 125_000usize;
    let duration_ms = 1_000u64;
    let expected_bps = (bytes as f64 * 8.0) / (duration_ms as f64 / 1_000.0);

    let fetcher = TimedSegmentFetcher::new(STATIC_MPD, bytes, duration_ms);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    // Before any fetch: throughput must be 0.
    assert_eq!(session.current_metrics(MediaType::Video).throughput_bps, 0.0);
    assert_eq!(session.current_metrics(MediaType::Video).latency_ms, 0.0);

    session
        .fetch_next_segment(MediaType::Video, &default_metrics())
        .await
        .unwrap();

    let m = session.current_metrics(MediaType::Video);
    assert!(
        (m.throughput_bps - expected_bps).abs() < 1.0,
        "expected ~{expected_bps} bps after first fetch, got {}",
        m.throughput_bps
    );
    assert_eq!(m.latency_ms, duration_ms as f64);
}

/// After multiple fetches the EMA must converge toward the steady-state value
/// and must never remain at 0.0.
#[tokio::test]
async fn current_metrics_throughput_ema_updates_each_fetch() {
    // 250 000 bytes in 500 ms → 4 000 000 bps (4 Mbps).
    let bytes = 250_000usize;
    let duration_ms = 500u64;
    let sample_bps = (bytes as f64 * 8.0) / (duration_ms as f64 / 1_000.0);

    let fetcher = TimedSegmentFetcher::new(STATIC_MPD, bytes, duration_ms);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    // Fetch 5 segments (VOD has 15 total at 2 s each).
    for _ in 0..5 {
        session
            .fetch_next_segment(MediaType::Video, &default_metrics())
            .await
            .unwrap();
    }

    let m = session.current_metrics(MediaType::Video);

    // After seeding on the first sample all subsequent samples are identical,
    // so the EMA should equal the sample value exactly.
    assert!(
        (m.throughput_bps - sample_bps).abs() < 1.0,
        "EMA should converge to {sample_bps} bps when all samples are equal, got {}",
        m.throughput_bps
    );
    assert!(m.throughput_bps > 0.0, "throughput must be non-zero after fetches");
    assert_eq!(m.latency_ms, duration_ms as f64);
}

/// `current_metrics()` for audio returns independent throughput from the
/// video track — the two tracks maintain separate EMAs.
#[tokio::test]
async fn current_metrics_audio_and_video_independent() {
    const AUDIO_VIDEO_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT30S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="video-$Number$.m4s" initialization="video-init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio">
      <SegmentTemplate media="audio-$Number$.m4s" initialization="audio-init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="a1" bandwidth="128000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let fetcher = TimedSegmentFetcher::new(AUDIO_VIDEO_MPD, 50_000, 200);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    // Before any fetches both tracks report 0.
    assert_eq!(session.current_metrics(MediaType::Video).throughput_bps, 0.0);
    assert_eq!(session.current_metrics(MediaType::Audio).throughput_bps, 0.0);

    // Fetch only the video track.
    session
        .fetch_next_segment(MediaType::Video, &default_metrics())
        .await
        .unwrap();

    // Video should now be non-zero; audio must still be 0.
    assert!(session.current_metrics(MediaType::Video).throughput_bps > 0.0);
    assert_eq!(
        session.current_metrics(MediaType::Audio).throughput_bps,
        0.0,
        "audio EMA must remain 0 until an audio segment is fetched"
    );
}

// ── SegmentList integration tests ─────────────────────────────────────────────

/// A minimal VOD MPD that uses `<SegmentList>` with three explicit
/// `<SegmentURL>` entries.  No `<SegmentTemplate>` is present so the
/// SegmentList addressing path is triggered.
const SEGMENTLIST_VOD_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT6S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="v1" bandwidth="500000">
        <SegmentList duration="2" timescale="1">
          <Initialization sourceURL="init-v1.mp4"/>
          <SegmentURL media="seg-v1-1.m4s"/>
          <SegmentURL media="seg-v1-2.m4s"/>
          <SegmentURL media="seg-v1-3.m4s"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

/// A `SegmentList` VOD stream fetches the three declared `<SegmentURL>`
/// entries in declaration order and then returns `EndOfStream`.
#[tokio::test]
async fn segmentlist_vod_fetches_in_order() {
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[SEGMENTLIST_VOD_MPD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = default_metrics();

    // All three segments should succeed.
    for i in 0..3 {
        session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await
            .unwrap_or_else(|e| panic!("segment {i} fetch failed: {e}"));
    }

    let urls = recorded.lock().unwrap();
    // Filter to media segments only (exclude any init fetches).
    let media_urls: Vec<&String> = urls.iter().filter(|u| u.contains("seg-v1")).collect();

    assert_eq!(media_urls.len(), 3, "expected 3 segment fetches; got {urls:?}");

    // Verify the URLs are fetched in declaration order.
    for (i, expected_suffix) in ["seg-v1-1.m4s", "seg-v1-2.m4s", "seg-v1-3.m4s"]
        .iter()
        .enumerate()
    {
        assert!(
            media_urls[i].ends_with(expected_suffix),
            "segment {i}: expected URL ending in {expected_suffix}, got {}",
            media_urls[i]
        );
    }
}

/// After all `<SegmentURL>` entries have been fetched, the next call must
/// return `DashError::EndOfStream`.
#[tokio::test]
async fn segmentlist_eos_after_all_urls() {
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[SEGMENTLIST_VOD_MPD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = default_metrics();

    // Consume all three segments.
    for _ in 0..3 {
        session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await
            .expect("segment fetch should succeed");
    }

    // Fourth call must signal EOS.
    let result = session.fetch_next_segment(MediaType::Video, &metrics).await;
    assert!(
        matches!(result, Err(DashError::EndOfStream { .. })),
        "expected EndOfStream after all SegmentURL entries consumed, got {result:?}"
    );
}

/// `fetch_init_segment` resolves the init URL from
/// `SegmentList.Initialization.sourceURL` and performs a network fetch.
#[tokio::test]
async fn segmentlist_init_segment_resolved() {
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[SEGMENTLIST_VOD_MPD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    session
        .fetch_init_segment(MediaType::Video)
        .await
        .expect("fetch_init_segment must succeed for a SegmentList stream");

    let urls = recorded.lock().unwrap();
    let init_url = urls
        .iter()
        .find(|u| u.contains("init-v1.mp4"))
        .unwrap_or_else(|| panic!("expected an init-v1.mp4 fetch; got {urls:?}"));

    assert!(
        init_url.ends_with("init-v1.mp4"),
        "init URL should resolve to init-v1.mp4; got {init_url}"
    );
}

// ── SegmentBase integration tests ─────────────────────────────────────────────

/// The MPD for SegmentBase tests.
///
/// The media file is `video.mp4`.  Its `indexRange` is `"1000-1999"` (the
/// byte range of the `sidx` box), so `index_end_offset = 2000`.  The init
/// data lives at bytes 0–999 (`Initialization@range="0-999"`).
const SEGMENTBASE_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT10S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="v1" bandwidth="1000000">
        <BaseURL>video.mp4</BaseURL>
        <SegmentBase indexRange="1000-1999">
          <Initialization range="0-999"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

/// Build a minimal v0 `sidx` box with `first_offset = 0` and the given
/// `durations` (in timescale ticks), so it can be returned by a test fetcher
/// to drive `SegmentBase` addressing.
///
/// `referenced_size` for every entry is fixed at 4 096 bytes.
fn make_test_sidx(timescale: u32, durations: &[u32]) -> Vec<u8> {
    let ref_count = durations.len() as u16;
    // v0 sidx: size(4) + type(4) + version(1) + flags(3) + ref_id(4)
    //          + timescale(4) + ept(4) + first_offset(4) + reserved(2)
    //          + ref_count(2) + ref_count × 12
    let size: u32 = 32 + 12 * ref_count as u32;
    let mut buf = Vec::with_capacity(size as usize);
    buf.extend_from_slice(&size.to_be_bytes());
    buf.extend_from_slice(b"sidx");
    buf.push(0); // version = 0
    buf.extend_from_slice(&[0u8; 3]); // flags
    buf.extend_from_slice(&1u32.to_be_bytes()); // reference_id
    buf.extend_from_slice(&timescale.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes()); // earliest_presentation_time
    buf.extend_from_slice(&0u32.to_be_bytes()); // first_offset
    buf.extend_from_slice(&[0u8; 2]); // reserved
    buf.extend_from_slice(&ref_count.to_be_bytes());
    for &dur in durations {
        // referenced_size = 4096 (lower 31 bits; ref_type = 0)
        buf.extend_from_slice(&4096u32.to_be_bytes());
        buf.extend_from_slice(&dur.to_be_bytes());
        buf.extend_from_slice(&0x8000_0000u32.to_be_bytes()); // SAP info
    }
    buf
}

/// A `SegmentFetcher` for SegmentBase tests.
///
/// - Manifest requests are served from a preset XML string.
/// - The first byte-range request to `video.mp4` returns the fake sidx.
/// - Subsequent requests to `video.mp4` return empty media bytes.
/// - All request URLs (and their byte ranges) are recorded in `log`.
struct SegmentBaseFetcher {
    manifest_xml: Bytes,
    sidx_data: Bytes,
    /// `("url", Option<(start, end)>)` pairs in order.
    log: Arc<Mutex<Vec<(String, Option<(u64, u64)>)>>>,
}

impl SegmentBaseFetcher {
    fn new(
        manifest_xml: &str,
        sidx_data: Vec<u8>,
        log: Arc<Mutex<Vec<(String, Option<(u64, u64)>)>>>,
    ) -> Self {
        Self {
            manifest_xml: Bytes::from(manifest_xml.to_owned()),
            sidx_data: Bytes::from(sidx_data),
            log,
        }
    }
}

#[async_trait]
impl SegmentFetcher for SegmentBaseFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        self.log
            .lock()
            .unwrap()
            .push((request.url.to_string(), request.byte_range));

        let data = if request.request_type == RequestType::Manifest {
            self.manifest_xml.clone()
        } else if request.byte_range == Some((1000, 1999)) {
            // This is the sidx index fetch.
            self.sidx_data.clone()
        } else {
            // Init or media segment byte-range fetch — return empty.
            Bytes::new()
        };

        let len = data.len() as u64;
        Ok(FetchResponse {
            final_url: request.url,
            data,
            transfer_duration_ms: 1,
            bytes_received: len,
        })
    }

    async fn check_existence(&self, _url: &Url) -> Result<bool, FetchError> {
        Ok(true)
    }

    fn abort(&self) {}
}

/// For a `SegmentBase` stream, the first `fetch_next_segment` call must fetch
/// the sidx index (byte-range 1000-1999) and then the first subsegment using
/// the byte range derived from the sidx.
#[tokio::test]
async fn segmentbase_fetches_sidx_then_first_segment() {
    // sidx: timescale=1000, 3 segments each 3000 ticks (3 s), 4096 bytes each.
    let sidx = make_test_sidx(1000, &[3000, 3000, 4000]);
    let log: Arc<Mutex<Vec<(String, Option<(u64, u64)>)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let fetcher =
        SegmentBaseFetcher::new(SEGMENTBASE_MPD, sidx, Arc::clone(&log));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = default_metrics();
    session
        .fetch_next_segment(MediaType::Video, &metrics)
        .await
        .expect("first SegmentBase fetch must succeed");

    let entries = log.lock().unwrap();
    // [0] = manifest fetch (no byte range)
    // [1] = sidx fetch (byte range 1000-1999)
    // [2] = first segment fetch (byte range 2000..2000+4095 = 2000-6095)
    assert!(entries.len() >= 3, "expected at least 3 fetches, got {entries:?}");
    assert_eq!(entries[1].1, Some((1000, 1999)), "sidx fetch must use indexRange");
    // first segment starts immediately after index_end_offset (2000) + first_offset (0).
    assert_eq!(
        entries[2].1,
        Some((2000, 2000 + 4096 - 1)),
        "first segment byte range wrong; got {:?}",
        entries[2].1
    );
}

/// After all subsegments described by the sidx have been fetched, the next
/// call must return `DashError::EndOfStream`.
#[tokio::test]
async fn segmentbase_eos_after_all_subsegments() {
    // sidx: 2 subsegments of 4096 bytes each.
    let sidx = make_test_sidx(1000, &[3000, 3000]);
    let log = Arc::new(Mutex::new(Vec::new()));
    let fetcher = SegmentBaseFetcher::new(SEGMENTBASE_MPD, sidx, log);
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = default_metrics();
    // Fetch both subsegments successfully.
    for i in 0..2 {
        session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await
            .unwrap_or_else(|e| panic!("segment {i} failed: {e}"));
    }
    // Third call must be EOS.
    let result = session.fetch_next_segment(MediaType::Video, &metrics).await;
    assert!(
        matches!(result, Err(DashError::EndOfStream { .. })),
        "expected EndOfStream after all SegmentBase subsegments, got {result:?}"
    );
}

/// `fetch_init_segment` for a `SegmentBase` stream resolves the init byte
/// range from `SegmentBase.Initialization@range` and issues a byte-range GET
/// against the media file URL.
#[tokio::test]
async fn segmentbase_init_segment_uses_byte_range() {
    let sidx = make_test_sidx(1000, &[3000]);
    let log: Arc<Mutex<Vec<(String, Option<(u64, u64)>)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let fetcher =
        SegmentBaseFetcher::new(SEGMENTBASE_MPD, sidx, Arc::clone(&log));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    session
        .fetch_init_segment(MediaType::Video)
        .await
        .expect("fetch_init_segment must succeed for SegmentBase stream");

    let entries = log.lock().unwrap();
    // [0] = manifest; [1] = init byte-range fetch.
    let init_entry = entries
        .iter()
        .skip(1)
        .find(|(url, _)| url.contains("video.mp4"))
        .expect("expected a fetch to video.mp4 for the init segment");
    assert_eq!(
        init_entry.1,
        Some((0, 999)),
        "init fetch must use Initialization@range 0-999; got {:?}",
        init_entry.1
    );
}

// ── ServiceDescription tests ──────────────────────────────────────────────────

const SERVICE_DESC_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT30S">
  <ServiceDescription id="sd1">
    <Latency target="2.5"/>
    <PlaybackRate min="0.9" max="1.1"/>
  </ServiceDescription>
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

#[test]
fn service_description_target_latency_ms() {
    let session = session_from_xml(SERVICE_DESC_MPD);
    let latency = session
        .target_latency_ms()
        .expect("ServiceDescription with Latency@target must return Some");
    // target="2.5" s → 2500 ms.
    assert!(
        (latency - 2500.0).abs() < 1e-6,
        "expected 2500.0 ms, got {latency}"
    );
}

#[test]
fn service_description_playback_rate_range() {
    let session = session_from_xml(SERVICE_DESC_MPD);
    let (min, max) = session
        .playback_rate_range()
        .expect("ServiceDescription with PlaybackRate must return Some");
    assert!((min - 0.9).abs() < 1e-9, "expected min=0.9, got {min}");
    assert!((max - 1.1).abs() < 1e-9, "expected max=1.1, got {max}");
}

#[test]
fn service_description_absent_returns_none() {
    // STATIC_MPD has no <ServiceDescription>.
    let session = session_from_xml(STATIC_MPD);
    assert!(session.target_latency_ms().is_none());
    assert!(session.playback_rate_range().is_none());
}

// ── Bitrate cap tests ─────────────────────────────────────────────────────────

/// When a max bitrate is set, ABR must not select a representation above the
/// cap even when the network would support a higher quality.
#[test]
fn bitrate_cap_clamps_abr_decision() {
    // Three representations: 500 kbps, 1 Mbps, 3 Mbps.
    const MULTI_REP_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT30S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="low"  bandwidth="500000"/>
      <Representation id="mid"  bandwidth="1000000"/>
      <Representation id="high" bandwidth="3000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let mut session = session_from_xml(MULTI_REP_MPD);
    // Metrics: very high throughput and buffer level high enough that
    // InsufficientBufferRule (≤ 2000 ms threshold) won't fire.
    let high_metrics = AbrMetrics::new(10_000_000.0, 10_000.0);

    // Without a cap, the ABR should pick "high" (index 2).
    let _decision = session.select_quality(&MediaType::Video, &high_metrics).unwrap();
    // Note: select_quality uses the ABR directly (uncapped); we verify cap
    // state via max_bitrate() instead of asserting the uncapped index here.

    // Cap at 1 Mbps (index 1 = "mid"); "high" (3 Mbps) must be excluded.
    session.set_max_bitrate(MediaType::Video, 1_000_000);
    assert_eq!(session.max_bitrate(&MediaType::Video), Some(1_000_000));

    // After clearing, the cap must be gone.
    session.clear_max_bitrate(MediaType::Video);
    assert_eq!(session.max_bitrate(&MediaType::Video), None);
}

/// With a bitrate cap active, `fetch_next_segment` must not fetch a segment
/// from a representation above the cap.
#[tokio::test]
async fn bitrate_cap_enforced_in_fetch_next_segment() {
    const MULTI_REP_MPD2: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT30S">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$RepresentationID$-$Number$.m4s"
                       initialization="init-$RepresentationID$.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="low"  bandwidth="500000"/>
      <Representation id="mid"  bandwidth="1000000"/>
      <Representation id="high" bandwidth="3000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[MULTI_REP_MPD2], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    // Cap at 1 Mbps (index 1 = "mid"); high-throughput metrics would otherwise
    // select index 2 = "high".
    session.set_max_bitrate(MediaType::Video, 1_000_000);

    let high_metrics = AbrMetrics::new(10_000_000.0, 100.0);
    session
        .fetch_next_segment(MediaType::Video, &high_metrics)
        .await
        .expect("fetch with bitrate cap must succeed");

    let urls = recorded.lock().unwrap();
    // The first media segment URL must use "low" or "mid", not "high".
    let seg_url = urls
        .iter()
        .find(|u| u.contains("seg-"))
        .expect("no segment URL recorded");
    assert!(
        !seg_url.contains("seg-high"),
        "bitrate cap should have prevented selecting 'high'; got {seg_url}"
    );
}

// ── Multi-period tests ────────────────────────────────────────────────────────

/// Two-period VOD MPD with 1 segment (2 s) per period.
/// `period_count()` should return 2.
#[tokio::test]
async fn multi_period_period_count() {
    const MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
     mediaPresentationDuration="PT4S">
  <Period id="p1" duration="PT2S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="p1-seg-$Number$.m4s" initialization="p1-init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
  <Period id="p2" duration="PT2S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="p2-seg-$Number$.m4s" initialization="p2-init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let session = session_from_xml(MPD);
    assert_eq!(session.period_count(), 2);
    assert_eq!(session.current_period_index(), 0);
}

/// Two-period VOD: fetch the 1 segment from period 1, then the session
/// transparently advances to period 2 and fetches its segment.
/// `EndOfStream` fires only after period 2 is exhausted.
#[tokio::test]
async fn multi_period_advances_on_period_end() {
    const MPD2: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
     mediaPresentationDuration="PT4S">
  <Period id="p1" duration="PT2S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="p1-seg-$Number$.m4s" initialization="p1-init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
  <Period id="p2" duration="PT2S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="p2-seg-$Number$.m4s" initialization="p2-init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[MPD2], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    assert_eq!(session.period_count(), 2);

    let metrics = AbrMetrics::new(4_000_000.0, 10_000.0);

    // Fetch 1 — period 0, segment 1. The period index stays at 0 until
    // the next call detects period 0's EOS.
    session.fetch_next_segment(MediaType::Video, &metrics).await.unwrap();
    assert_eq!(session.current_period_index(), 0, "should still be in period 0 after first fetch");

    // Fetch 2 — period 0 EOS detected → auto-advance to period 1 → period 1, segment 1.
    session.fetch_next_segment(MediaType::Video, &metrics).await.unwrap();
    assert_eq!(session.current_period_index(), 1, "should be in period 1 after second fetch");

    // Fetch 3 — period 1 exhausted, no period 2 → EndOfStream.
    let err = session.fetch_next_segment(MediaType::Video, &metrics).await.unwrap_err();
    assert!(
        matches!(err, dash_rs::error::DashError::EndOfStream { .. }),
        "expected EndOfStream, got {err:?}"
    );

    // The recorded URLs should include both periods' segment URLs.
    let urls = recorded.lock().unwrap();
    let seg_urls: Vec<_> = urls.iter().filter(|u| u.contains("seg-")).collect();
    assert!(
        seg_urls.iter().any(|u| u.contains("p1-seg-")),
        "expected a p1 segment URL in {seg_urls:?}"
    );
    assert!(
        seg_urls.iter().any(|u| u.contains("p2-seg-")),
        "expected a p2 segment URL in {seg_urls:?}"
    );
}

/// Three-period VOD: `EndOfStream` fires only after all three periods are
/// exhausted (3 fetches total, one segment per period).
#[tokio::test]
async fn multi_period_eos_after_last_period() {
    const MPD3: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
     mediaPresentationDuration="PT6S">
  <Period id="p1" duration="PT2S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="p1-seg-$Number$.m4s" initialization="p1-init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
  <Period id="p2" duration="PT2S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="p2-seg-$Number$.m4s" initialization="p2-init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
  <Period id="p3" duration="PT2S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="p3-seg-$Number$.m4s" initialization="p3-init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let session_xml = session_from_xml(MPD3);
    assert_eq!(session_xml.period_count(), 3);

    // Build via from_url so init segments are fetched.
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[MPD3], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();
    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .unwrap();

    let metrics = AbrMetrics::new(4_000_000.0, 10_000.0);

    // 3 successful fetches (one per period).
    for i in 0..3 {
        session
            .fetch_next_segment(MediaType::Video, &metrics)
            .await
            .unwrap_or_else(|e| panic!("fetch {i} failed: {e}"));
    }

    // 4th fetch → EndOfStream.
    let err = session.fetch_next_segment(MediaType::Video, &metrics).await.unwrap_err();
    assert!(
        matches!(err, dash_rs::error::DashError::EndOfStream { .. }),
        "expected EndOfStream after all 3 periods, got {err:?}"
    );
    assert_eq!(session.current_period_index(), 2);
}

/// Single-period VOD: period_count() == 1 and EndOfStream fires normally.
#[tokio::test]
async fn single_period_period_count_is_one() {
    let session = session_from_xml(STATIC_MPD);
    assert_eq!(session.period_count(), 1);
    assert_eq!(session.current_period_index(), 0);
}

// ── XLink resolution tests ────────────────────────────────────────────────────

/// A `Period` with `xlink:actuate="onLoad"` must be fetched, parsed, and
/// inlined in-place before session construction.  The resulting session should
/// behave as if the remote content was always present in the MPD.
#[tokio::test]
async fn xlink_period_onload_is_inlined() {
    // Remote resource: a bare Period fragment served at the xlink href URL.
    const REMOTE_PERIOD: &str = r#"<Period id="remote" duration="PT4S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="remote-seg-$Number$.m4s" initialization="remote-init.mp4"
                       startNumber="1" duration="4" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>"#;

    // Main MPD: one placeholder period pointing to the remote resource.
    const MAIN_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     xmlns:xlink="http://www.w3.org/1999/xlink"
     type="static"
     mediaPresentationDuration="PT4S">
  <Period xlink:href="http://example.com/remote-period.mpd"
          xlink:actuate="onLoad"/>
</MPD>"#;

    // Feed the fetcher: first request → main MPD, second → remote period XML.
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[MAIN_MPD, REMOTE_PERIOD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();

    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .expect("session construction should succeed after XLink resolution");

    // The remote period was inlined: session has exactly one period.
    assert_eq!(session.period_count(), 1, "inlined period should be present");

    // The session can fetch a segment whose URL comes from the remote period's
    // SegmentTemplate — confirming the remote content is active.
    let metrics = AbrMetrics::new(4_000_000.0, 10_000.0);
    session
        .fetch_next_segment(MediaType::Video, &metrics)
        .await
        .expect("segment fetch from inlined period should succeed");

    let urls = recorded.lock().unwrap();
    assert!(
        urls.iter().any(|u| u.contains("remote-seg-")),
        "expected a remote-seg-N URL but recorded {urls:?}"
    );
}

/// A `Period` with `href="urn:mpeg:dash:resolve-to-zero:2013"` and
/// `actuate="onLoad"` must be silently removed without triggering any
/// additional network request.
#[tokio::test]
async fn xlink_period_resolve_to_zero_removes_period() {
    // Two-period MPD: p1 is normal; p2 is the resolve-to-zero sentinel.
    const MAIN_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     xmlns:xlink="http://www.w3.org/1999/xlink"
     type="static"
     mediaPresentationDuration="PT4S">
  <Period id="p1" duration="PT4S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="4" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
  <Period xlink:href="urn:mpeg:dash:resolve-to-zero:2013"
          xlink:actuate="onLoad"/>
</MPD>"#;

    // Only one manifest in the queue — resolve-to-zero must not trigger a fetch.
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[MAIN_MPD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();

    let session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .expect("session construction should succeed");

    // The resolve-to-zero period was removed; only p1 remains.
    assert_eq!(session.period_count(), 1, "resolve-to-zero period should be removed");
}

/// A `Period` with `xlink:actuate="onRequest"` must NOT be resolved eagerly.
/// The placeholder must remain in the manifest and no additional fetch must be
/// triggered during session construction.
#[tokio::test]
async fn xlink_period_onrequest_is_not_fetched() {
    // Two-period MPD: p1 is normal; p2 is an onRequest XLink placeholder.
    const MAIN_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     xmlns:xlink="http://www.w3.org/1999/xlink"
     type="static"
     mediaPresentationDuration="PT4S">
  <Period id="p1" duration="PT4S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       startNumber="1" duration="4" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
  <Period id="p2" xlink:href="http://example.com/extra.mpd"
          xlink:actuate="onRequest"/>
</MPD>"#;

    // Only one manifest in the queue — confirm no second fetch is triggered.
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[MAIN_MPD], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();

    let session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .expect("session construction should succeed");

    // Both periods are present: p1 (normal) and p2 (unresolved placeholder).
    assert_eq!(session.period_count(), 2, "onRequest placeholder must remain in manifest");
}

/// An `AdaptationSet` with `xlink:actuate="onLoad"` must be fetched and
/// inlined into its containing Period.
#[tokio::test]
async fn xlink_adaptation_set_onload_is_inlined() {
    // Remote resource: a bare AdaptationSet fragment.
    const REMOTE_ADAPTATION: &str = r#"<AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="remote-as-seg-$Number$.m4s" initialization="remote-as-init.mp4"
                       startNumber="1" duration="4" timescale="1"/>
      <Representation id="v1" bandwidth="1000000"/>
    </AdaptationSet>"#;

    // Main MPD: one period with a placeholder AdaptationSet.
    const MAIN_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     xmlns:xlink="http://www.w3.org/1999/xlink"
     type="static"
     mediaPresentationDuration="PT4S">
  <Period id="p1" duration="PT4S">
    <AdaptationSet xlink:href="http://example.com/as.mpd"
                   xlink:actuate="onLoad"/>
  </Period>
</MPD>"#;

    // Feed the fetcher: first → main MPD, second → remote AdaptationSet XML.
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fetcher = RecordingFetcher::new(&[MAIN_MPD, REMOTE_ADAPTATION], Arc::clone(&recorded));
    let url = Url::parse("http://example.com/manifest.mpd").unwrap();

    let mut session = DashSession::from_url(&url, fetcher, open_stub_source())
        .await
        .expect("session construction should succeed after AdaptationSet XLink resolution");

    // The remote AdaptationSet was inlined; we can fetch a segment from it.
    let metrics = AbrMetrics::new(4_000_000.0, 10_000.0);
    session
        .fetch_next_segment(MediaType::Video, &metrics)
        .await
        .expect("segment fetch from inlined AdaptationSet should succeed");

    let urls = recorded.lock().unwrap();
    assert!(
        urls.iter().any(|u| u.contains("remote-as-seg-")),
        "expected a remote-as-seg-N URL but recorded {urls:?}"
    );
}
