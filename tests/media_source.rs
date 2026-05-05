//! Integration tests for the MSE abstraction layer.

use bytes::Bytes;
use dash_rs::{
    error::DashError,
    media_source::{
        stub::{StubMediaSource, StubSourceBuffer},
        Chunk, MediaSource, ReadyState, SourceBuffer,
    },
};

// ── StubMediaSource lifecycle ─────────────────────────────────────────────────

#[test]
fn stub_media_source_open_close() {
    let mut ms = StubMediaSource::new();
    assert_eq!(ms.ready_state(), ReadyState::Closed);

    ms.open();
    assert_eq!(ms.ready_state(), ReadyState::Open);

    ms.close();
    assert_eq!(ms.ready_state(), ReadyState::Closed);
}

#[test]
fn stub_media_source_add_source_buffer_requires_open() {
    let mut ms = StubMediaSource::new();
    // Not open yet — should error.
    let result = ms.add_source_buffer("video/mp4");
    assert!(matches!(result, Err(DashError::MediaSourceNotOpen { .. })));
}

#[test]
fn stub_media_source_add_and_remove_source_buffer() {
    let mut ms = StubMediaSource::new();
    ms.open();

    ms.add_source_buffer("video/mp4").unwrap();
    ms.remove_source_buffer("video/mp4").unwrap();

    // Removing a non-existent buffer should error.
    let result = ms.remove_source_buffer("video/mp4");
    assert!(matches!(result, Err(DashError::SourceBufferNotFound { .. })));
}

#[test]
fn stub_media_source_set_duration() {
    let mut ms = StubMediaSource::new();
    ms.open();
    ms.set_duration(60.0).unwrap();
    assert_eq!(ms.duration(), Some(60.0));
}

#[tokio::test]
async fn stub_media_source_end_of_stream() {
    let mut ms = StubMediaSource::new();
    ms.open();
    ms.end_of_stream().await.unwrap();
    assert_eq!(ms.ready_state(), ReadyState::Ended);
}

// ── StubSourceBuffer ──────────────────────────────────────────────────────────

#[tokio::test]
async fn stub_source_buffer_append_and_buffered() {
    let mut buf = StubSourceBuffer::new("video/mp4");
    assert!(!buf.is_updating());
    assert!(buf.buffered().is_empty());

    let chunk = Chunk::new(Bytes::from_static(b"\x00\x01\x02"), Some(0.0), Some(4.0), "seg-1");
    buf.append_buffer(chunk).await.unwrap();

    let ranges = buf.buffered();
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].start, 0.0);
    assert_eq!(ranges[0].end, 4.0);
}

#[tokio::test]
async fn stub_source_buffer_merges_adjacent_ranges() {
    let mut buf = StubSourceBuffer::new("video/mp4");

    buf.append_buffer(Chunk::new(Bytes::from_static(b"a"), Some(0.0), Some(4.0), "seg-1"))
        .await
        .unwrap();
    buf.append_buffer(Chunk::new(Bytes::from_static(b"b"), Some(4.0), Some(8.0), "seg-2"))
        .await
        .unwrap();

    // Adjacent segments should be merged into one range.
    let ranges = buf.buffered();
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].start, 0.0);
    assert_eq!(ranges[0].end, 8.0);
}

#[tokio::test]
async fn stub_source_buffer_keeps_discontinuous_ranges_separate() {
    let mut buf = StubSourceBuffer::new("video/mp4");

    buf.append_buffer(Chunk::new(Bytes::from_static(b"a"), Some(0.0), Some(4.0), "seg-1"))
        .await
        .unwrap();
    // Gap: segment 2 is missing.
    buf.append_buffer(Chunk::new(Bytes::from_static(b"c"), Some(8.0), Some(12.0), "seg-3"))
        .await
        .unwrap();

    let ranges = buf.buffered();
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0].end, 4.0);
    assert_eq!(ranges[1].start, 8.0);
}

#[tokio::test]
async fn stub_source_buffer_remove_clears_range() {
    let mut buf = StubSourceBuffer::new("video/mp4");

    for i in 0u32..5 {
        let start = (i * 4) as f64;
        let end = start + 4.0;
        buf.append_buffer(Chunk::new(Bytes::from_static(b"x"), Some(start), Some(end), "seg"))
            .await
            .unwrap();
    }

    // Remove the first 8 seconds.
    buf.remove(0.0, 8.0).await.unwrap();

    let ranges = buf.buffered();
    // Only the segments from 8 s onwards should remain.
    assert!(ranges.iter().all(|r| r.start >= 8.0));
}

#[tokio::test]
async fn stub_source_buffer_init_segment_not_tracked_in_time_ranges() {
    let mut buf = StubSourceBuffer::new("video/mp4");

    // Init segment: no timing information.
    let init = Chunk::new(Bytes::from_static(b"moov"), None, None, "init");
    buf.append_buffer(init).await.unwrap();

    // No time range recorded, but no error either.
    assert!(buf.buffered().is_empty());
}

#[tokio::test]
async fn stub_source_buffer_abort_clears_updating() {
    let mut buf = StubSourceBuffer::new("video/mp4");
    buf.abort().await.unwrap();
    assert!(!buf.is_updating());
}

#[tokio::test]
async fn stub_source_buffer_change_type() {
    let mut buf = StubSourceBuffer::new("video/mp4");
    buf.change_type("video/webm").await.unwrap();
    assert_eq!(buf.mime_type(), "video/webm");
}

// ── SourceBufferSink ──────────────────────────────────────────────────────────

#[tokio::test]
async fn source_buffer_sink_append_and_abort() {
    use dash_rs::media_source::sink::SourceBufferSink;

    let inner = Box::new(StubSourceBuffer::new("audio/mp4"));
    let mut sink = SourceBufferSink::new(inner);

    let chunk = Chunk::new(Bytes::from_static(b"\xFF"), Some(0.0), Some(2.0), "audio/1");
    sink.append(chunk).await.unwrap();

    let ranges = sink.buffered();
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].end, 2.0);

    sink.abort().await.unwrap();
}

#[tokio::test]
async fn source_buffer_sink_remove_validates_range() {
    use dash_rs::media_source::sink::SourceBufferSink;

    let inner = Box::new(StubSourceBuffer::new("audio/mp4"));
    let mut sink = SourceBufferSink::new(inner);

    // Invalid range (end ≤ start) should return an error.
    let result = sink.remove(5.0, 3.0).await;
    assert!(matches!(result, Err(DashError::InvalidTimeRange { .. })));
}
