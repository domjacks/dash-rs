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
fn session_from_xml(xml: &str) -> DashSession<dash_rs::fetch::stub::StubSegmentFetcher, StubMediaSource> {
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
    assert_eq!(session_from_xml(STATIC_MPD).minimum_update_period_ms(), None);
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
    let result = session
        .fetch_next_segment(MediaType::Video, &metrics)
        .await;
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
    assert_eq!(urls.len(), 1, "exactly one segment should have been fetched");

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
