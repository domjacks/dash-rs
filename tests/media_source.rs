//! Integration tests for the MSE abstraction layer.

use bytes::Bytes;
use dash_rs::{
    error::DashError,
    media_source::{
        sink::StreamInfo,
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
    assert!(matches!(
        result,
        Err(DashError::SourceBufferNotFound { .. })
    ));
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

    let chunk = Chunk::new(
        Bytes::from_static(b"\x00\x01\x02"),
        Some(0.0),
        Some(4.0),
        "seg-1",
    );
    buf.append_buffer(chunk).await.unwrap();

    let ranges = buf.buffered();
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].start, 0.0);
    assert_eq!(ranges[0].end, 4.0);
}

#[tokio::test]
async fn stub_source_buffer_merges_adjacent_ranges() {
    let mut buf = StubSourceBuffer::new("video/mp4");

    buf.append_buffer(Chunk::new(
        Bytes::from_static(b"a"),
        Some(0.0),
        Some(4.0),
        "seg-1",
    ))
    .await
    .unwrap();
    buf.append_buffer(Chunk::new(
        Bytes::from_static(b"b"),
        Some(4.0),
        Some(8.0),
        "seg-2",
    ))
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

    buf.append_buffer(Chunk::new(
        Bytes::from_static(b"a"),
        Some(0.0),
        Some(4.0),
        "seg-1",
    ))
    .await
    .unwrap();
    // Gap: segment 2 is missing.
    buf.append_buffer(Chunk::new(
        Bytes::from_static(b"c"),
        Some(8.0),
        Some(12.0),
        "seg-3",
    ))
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
        buf.append_buffer(Chunk::new(
            Bytes::from_static(b"x"),
            Some(start),
            Some(end),
            "seg",
        ))
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

#[tokio::test]
async fn source_buffer_sink_set_timestamp_offset_stored_and_retrieved() {
    use dash_rs::media_source::sink::SourceBufferSink;

    let inner = Box::new(StubSourceBuffer::new("video/mp4"));
    let mut sink = SourceBufferSink::new(inner);

    // Default offset is 0.0.
    assert_eq!(sink.timestamp_offset(), 0.0);

    sink.set_timestamp_offset(5.0);
    assert_eq!(sink.timestamp_offset(), 5.0);
}

#[tokio::test]
async fn source_buffer_sink_set_timestamp_offset_no_op_when_unchanged() {
    use dash_rs::media_source::sink::SourceBufferSink;

    let inner = Box::new(StubSourceBuffer::new("video/mp4"));
    let mut sink = SourceBufferSink::new(inner);

    sink.set_timestamp_offset(3.0);
    assert_eq!(sink.timestamp_offset(), 3.0);

    // Calling again with the same value should be a no-op — offset stays 3.0.
    sink.set_timestamp_offset(3.0);
    assert_eq!(sink.timestamp_offset(), 3.0);

    // Non-finite values must be silently ignored.
    sink.set_timestamp_offset(f64::NAN);
    assert_eq!(sink.timestamp_offset(), 3.0);

    sink.set_timestamp_offset(f64::INFINITY);
    assert_eq!(sink.timestamp_offset(), 3.0);
}

// ── SourceBufferSink::update_append_window ────────────────────────────────────

#[tokio::test]
async fn update_append_window_basic() {
    use dash_rs::media_source::sink::SourceBufferSink;

    let inner = Box::new(StubSourceBuffer::new("video/mp4"));
    let mut sink = SourceBufferSink::new(inner);

    // period start=10 s, duration=30 s →
    //   appendWindowStart = max(10 − 0.1, 0) = 9.9
    //   appendWindowEnd   = 10 + 30 + 0.01 = 40.01
    sink.update_append_window(
        &StreamInfo {
            start: 10.0,
            duration: Some(30.0),
        },
        None,
    );

    assert!(
        (sink.append_window_start() - 9.9).abs() < 1e-9,
        "appendWindowStart should be 9.9, got {}",
        sink.append_window_start()
    );
    assert!(
        (sink.append_window_end() - 40.01).abs() < 1e-9,
        "appendWindowEnd should be 40.01, got {}",
        sink.append_window_end()
    );
}

#[tokio::test]
async fn update_append_window_clamps_start_at_zero() {
    use dash_rs::media_source::sink::SourceBufferSink;

    let inner = Box::new(StubSourceBuffer::new("video/mp4"));
    let mut sink = SourceBufferSink::new(inner);

    // period start=0.05 s → start − 0.1 = −0.05 → clamped to 0.0
    sink.update_append_window(
        &StreamInfo {
            start: 0.05,
            duration: Some(10.0),
        },
        None,
    );

    assert_eq!(
        sink.append_window_start(),
        0.0,
        "appendWindowStart must be clamped to 0"
    );
}

#[tokio::test]
async fn update_append_window_no_duration_falls_back_to_source_duration() {
    use dash_rs::media_source::sink::SourceBufferSink;

    let inner = Box::new(StubSourceBuffer::new("video/mp4"));
    let mut sink = SourceBufferSink::new(inner);

    // No period duration → use media_source_duration.
    // appendWindowEnd = 60.0 + 0.01 = 60.01
    sink.update_append_window(
        &StreamInfo {
            start: 0.0,
            duration: None,
        },
        Some(60.0),
    );

    assert!(
        (sink.append_window_end() - 60.01).abs() < 1e-9,
        "appendWindowEnd should fall back to media_source_duration + offset, got {}",
        sink.append_window_end()
    );
}

#[tokio::test]
async fn update_append_window_no_duration_no_source_duration_is_infinity() {
    use dash_rs::media_source::sink::SourceBufferSink;

    let inner = Box::new(StubSourceBuffer::new("video/mp4"));
    let mut sink = SourceBufferSink::new(inner);

    sink.update_append_window(
        &StreamInfo {
            start: 0.0,
            duration: None,
        },
        None,
    );

    assert_eq!(
        sink.append_window_end(),
        f64::INFINITY,
        "appendWindowEnd should be ∞ when no duration is known"
    );
}

#[tokio::test]
async fn update_append_window_infinite_period_duration_falls_back() {
    use dash_rs::media_source::sink::SourceBufferSink;

    let inner = Box::new(StubSourceBuffer::new("video/mp4"));
    let mut sink = SourceBufferSink::new(inner);

    // Infinite period duration treated like None → use media_source_duration.
    sink.update_append_window(
        &StreamInfo {
            start: 0.0,
            duration: Some(f64::INFINITY),
        },
        Some(30.0),
    );

    assert!(
        (sink.append_window_end() - 30.01).abs() < 1e-9,
        "infinite period duration should fall back to media_source_duration, got {}",
        sink.append_window_end()
    );
}

// ── SourceBufferSink::abort append-window restoration ─────────────────────────

#[tokio::test]
async fn sink_abort_restores_append_window() {
    use dash_rs::media_source::sink::SourceBufferSink;

    let inner = Box::new(StubSourceBuffer::new("video/mp4"));
    let mut sink = SourceBufferSink::new(inner);

    // Set a non-default append window.
    sink.update_append_window(
        &StreamInfo {
            start: 5.0,
            duration: Some(20.0),
        },
        None,
    );
    let saved_start = sink.append_window_start();
    let saved_end = sink.append_window_end();

    // MSE abort() resets the window; our wrapper must restore it.
    sink.abort().await.unwrap();

    assert!(
        (sink.append_window_start() - saved_start).abs() < 1e-9,
        "append_window_start should be restored after abort, got {}",
        sink.append_window_start()
    );
    assert!(
        (sink.append_window_end() - saved_end).abs() < 1e-9,
        "append_window_end should be restored after abort, got {}",
        sink.append_window_end()
    );
}

// ── StubSourceBuffer::remove semantics ───────────────────────────────────────

#[tokio::test]
async fn stub_remove_exact_match_removes_chunk() {
    let mut buf = StubSourceBuffer::new("video/mp4");

    buf.append_buffer(Chunk::new(
        Bytes::from_static(b"data"),
        Some(2.0),
        Some(4.0),
        "seg",
    ))
    .await
    .unwrap();

    // Remove exactly the buffered range.
    buf.remove(2.0, 4.0).await.unwrap();
    assert!(
        buf.buffered().is_empty(),
        "exact-range removal should clear all buffered data"
    );
}

#[tokio::test]
async fn stub_remove_non_overlapping_preserved() {
    let mut buf = StubSourceBuffer::new("video/mp4");

    // Two non-overlapping chunks.
    buf.append_buffer(Chunk::new(
        Bytes::from_static(b"a"),
        Some(0.0),
        Some(2.0),
        "s1",
    ))
    .await
    .unwrap();
    buf.append_buffer(Chunk::new(
        Bytes::from_static(b"b"),
        Some(8.0),
        Some(10.0),
        "s2",
    ))
    .await
    .unwrap();

    // Remove a range that overlaps neither chunk.
    buf.remove(3.0, 6.0).await.unwrap();

    let ranges = buf.buffered();
    assert_eq!(ranges.len(), 2, "both non-overlapping chunks should remain");
    assert_eq!(ranges[0].start, 0.0);
    assert_eq!(ranges[1].start, 8.0);
}
