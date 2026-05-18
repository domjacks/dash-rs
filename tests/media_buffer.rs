//! Integration tests for the `MediaBuffer` abstraction layer.

use bytes::Bytes;
use dash_rs::{
    media_buffer::{MediaBuffer, NullBuffer, StubBuffer, TimeRange},
    session::MediaType,
};

// ── NullBuffer ────────────────────────────────────────────────────────────────

#[test]
fn null_buffer_add_track_always_returns_true() {
    let mut buf = NullBuffer;
    assert!(buf.add_track(MediaType::Video, "video/mp4; codecs=\"avc1\""));
    assert!(buf.add_track(MediaType::Audio, "audio/mp4; codecs=\"mp4a.40.2\""));
}

#[test]
fn null_buffer_buffered_always_empty() {
    let buf = NullBuffer;
    assert!(buf.buffered(MediaType::Video).is_empty());
    assert!(buf.buffered(MediaType::Audio).is_empty());
}

#[tokio::test]
async fn null_buffer_append_is_noop() {
    let mut buf = NullBuffer;
    let result = buf
        .append(MediaType::Video, Bytes::from_static(b"\x00\x00\x00"), true)
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn null_buffer_end_of_stream_is_noop() {
    let mut buf = NullBuffer;
    assert!(buf.end_of_stream().await.is_ok());
}

#[tokio::test]
async fn null_buffer_open_is_noop() {
    let mut buf = NullBuffer;
    assert!(buf.open().await.is_ok());
}

// ── StubBuffer ────────────────────────────────────────────────────────────────

#[test]
fn stub_buffer_new_has_default_append_window() {
    let buf = StubBuffer::new();
    assert_eq!(buf.video_append_window, (0.0, f64::INFINITY));
    assert_eq!(buf.audio_append_window, (0.0, f64::INFINITY));
}

#[test]
fn stub_buffer_set_duration() {
    let mut buf = StubBuffer::new();
    assert_eq!(buf.duration, None);
    buf.set_duration(120.0);
    assert_eq!(buf.duration, Some(120.0));
}

#[test]
fn stub_buffer_set_live_mode() {
    let mut buf = StubBuffer::new();
    assert!(!buf.is_live);
    buf.set_live_mode(true);
    assert!(buf.is_live);
    buf.set_live_mode(false);
    assert!(!buf.is_live);
}

#[test]
fn stub_buffer_add_track_sets_mime_and_returns_true() {
    let mut buf = StubBuffer::new();
    assert!(buf.add_track(MediaType::Video, "video/mp4; codecs=\"avc1\""));
    assert_eq!(buf.video_mime, "video/mp4; codecs=\"avc1\"");
    assert!(buf.add_track(MediaType::Audio, "audio/mp4; codecs=\"mp4a.40.2\""));
    assert_eq!(buf.audio_mime, "audio/mp4; codecs=\"mp4a.40.2\"");
}

#[tokio::test]
async fn stub_buffer_change_type_updates_mime() {
    let mut buf = StubBuffer::new();
    buf.add_track(MediaType::Video, "video/mp4");
    buf.change_type(MediaType::Video, "video/webm").await.unwrap();
    assert_eq!(buf.video_mime, "video/webm");
}

#[test]
fn stub_buffer_set_timestamp_offset() {
    let mut buf = StubBuffer::new();
    buf.set_timestamp_offset(MediaType::Video, 3.5);
    assert_eq!(buf.video_timestamp_offset, 3.5);
    buf.set_timestamp_offset(MediaType::Audio, -1.0);
    assert_eq!(buf.audio_timestamp_offset, -1.0);
}

#[test]
fn stub_buffer_set_append_window() {
    let mut buf = StubBuffer::new();
    buf.set_append_window(MediaType::Video, 1.0, 60.0);
    assert_eq!(buf.video_append_window, (1.0, 60.0));
    buf.set_append_window(MediaType::Audio, 0.0, 30.0);
    assert_eq!(buf.audio_append_window, (0.0, 30.0));
}

#[tokio::test]
async fn stub_buffer_append_records_chunks() {
    let mut buf = StubBuffer::new();
    let init = Bytes::from_static(b"init-data");
    let media = Bytes::from_static(b"media-data");

    buf.append(MediaType::Video, init.clone(), true).await.unwrap();
    buf.append(MediaType::Video, media.clone(), false).await.unwrap();

    assert_eq!(buf.video_chunks.len(), 2);
    assert!(buf.video_chunks[0].is_init);
    assert!(!buf.video_chunks[1].is_init);
    assert_eq!(&buf.video_chunks[0].data, &init);
    assert_eq!(&buf.video_chunks[1].data, &media);
    assert_eq!(buf.audio_chunks.len(), 0);
}

#[tokio::test]
async fn stub_buffer_append_audio_separate_from_video() {
    let mut buf = StubBuffer::new();
    buf.append(MediaType::Video, Bytes::from_static(b"v"), false).await.unwrap();
    buf.append(MediaType::Audio, Bytes::from_static(b"a"), false).await.unwrap();
    assert_eq!(buf.video_chunks.len(), 1);
    assert_eq!(buf.audio_chunks.len(), 1);
}

#[test]
fn stub_buffer_bytes_appended() {
    let buf = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut b = StubBuffer::new();
        b.append(MediaType::Video, Bytes::from(vec![0u8; 100]), true).await.unwrap();
        b.append(MediaType::Video, Bytes::from(vec![0u8; 200]), false).await.unwrap();
        b.append(MediaType::Audio, Bytes::from(vec![0u8; 50]), true).await.unwrap();
        b
    });
    assert_eq!(buf.video_bytes_appended(), 300);
    assert_eq!(buf.audio_bytes_appended(), 50);
}

#[tokio::test]
async fn stub_buffer_end_of_stream_sets_ended() {
    let mut buf = StubBuffer::new();
    assert!(!buf.ended);
    buf.end_of_stream().await.unwrap();
    assert!(buf.ended);
}

#[test]
fn stub_buffer_buffered_always_empty() {
    let buf = StubBuffer::new();
    // StubBuffer does not parse fMP4 timing, so buffered() always returns [].
    assert!(buf.buffered(MediaType::Video).is_empty());
    assert!(buf.buffered(MediaType::Audio).is_empty());
}

#[tokio::test]
async fn stub_buffer_remove_range_is_noop() {
    let mut buf = StubBuffer::new();
    buf.append(MediaType::Video, Bytes::from_static(b"data"), false).await.unwrap();
    // remove_range is a no-op for StubBuffer (no timing tracking).
    buf.remove_range(MediaType::Video, 0.0, 10.0).await.unwrap();
    // Chunk is still there — stub doesn't remove it.
    assert_eq!(buf.video_chunks.len(), 1);
}

// ── TimeRange ─────────────────────────────────────────────────────────────────

#[test]
fn time_range_duration() {
    let r = TimeRange::new(1.0, 4.0);
    assert!((r.duration() - 3.0).abs() < 1e-9);
}

#[test]
fn time_range_equality() {
    assert_eq!(TimeRange::new(0.0, 5.0), TimeRange::new(0.0, 5.0));
    assert_ne!(TimeRange::new(0.0, 5.0), TimeRange::new(0.0, 6.0));
}

// ── MediaBuffer trait object safety ──────────────────────────────────────────

/// The trait can be used as a `Box<dyn MediaBuffer>` (object-safe because
/// all async methods are wrapped in `async-trait`).
#[test]
fn media_buffer_is_object_safe() {
    let _buf: Box<dyn MediaBuffer> = Box::new(NullBuffer);
    let _buf2: Box<dyn MediaBuffer> = Box::new(StubBuffer::new());
}
