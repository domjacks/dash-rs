//! Minimal ISO Base Media File Format (ISOBMFF) box parsing.
//!
//! This module implements only the subset of the spec needed to extract
//! presentation-time information and in-band timed events from MPEG-DASH
//! media segments:
//!
//! - [`parse_sidx`] — parses a `SegmentIndexBox` (`sidx`) to obtain the
//!   segment's start and end presentation times in seconds.
//! - [`parse_all_emsg`] — scans a segment for all `DASHEventMessageBox`
//!   (`emsg`) boxes (ISO 14496-12 §8.10.2) and returns one [`EmsgInfo`] per
//!   box found.
//!
//! # Why `sidx`?
//!
//! DASH media segments do **not** carry presentation-time metadata in any
//! container header that is trivially readable without a full demuxer.  The
//! `sidx` box (ISO 14496-12 §8.16) is the lightest-weight source of this
//! information: it contains:
//!
//! - `earliest_presentation_time` — the presentation time (in
//!   `timescale` ticks) of the first sample in the segment.
//! - One or more *reference entries*, each with a `subsegment_duration`
//!   field.  The sum of durations gives the total segment duration.
//!
//! With these two values we can compute `Chunk::start` and `Chunk::end` (in
//! seconds) for every fetched segment, which in turn lets
//! `StubSourceBuffer::buffered()` and `GstSourceBuffer::buffered()` return
//! accurate time ranges — enabling the `BufferOccupancyRule` to make
//! data-driven quality decisions instead of returning no opinion.
//!
//! # Why `emsg`?
//!
//! MPEG-DASH in-band events are carried as `emsg` boxes (ISO 14496-12
//! §8.10.2) embedded in media segments.  They are the mechanism used for
//! ad-insertion cues (SCTE-35), audience measurement triggers, and other
//! timed metadata.  Two box versions exist:
//!
//! | Version | Presentation time field | String fields order |
//! |---|---|---|
//! | 0 | `presentation_time_delta` (u32, relative to segment start) | strings first |
//! | 1 | `presentation_time` (u64, absolute on media timeline) | integers first |
//!
//! # Limitations
//!
//! - Only **top-level** `sidx` / `emsg` boxes are scanned.  Boxes nested
//!   inside `moof` fragments are not found.
//! - `sidx` boxes embedded inside `moof` are not found.
//! - Segments that use only `tfdt`/`mvhd` for timing (no `sidx`) will still
//!   produce `Chunk { start: None, end: None }` — this is the pre-existing
//!   behaviour and is not a regression.

use bytes::Bytes;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

// ── EmsgInfo / parse_all_emsg ─────────────────────────────────────────────────

/// A single DASH event parsed from a `DASHEventMessageBox` (`emsg`,
/// ISO 14496-12 §8.10.2).
///
/// Two wire versions exist:
///
/// | Field | Version 0 | Version 1 |
/// |---|---|---|
/// | `presentation_time_s` | `presentation_time_delta / timescale` — relative to the segment's earliest presentation time | `presentation_time / timescale` — absolute on the media timeline |
///
/// The DASH-IF recommends version 1 for new content; version 0 is still
/// widespread in live ad-insertion workflows (SCTE-35 `urn:scte:scte35:2014:xml+bin`).
#[derive(Debug, Clone, PartialEq)]
pub struct EmsgInfo {
    /// Box version: 0 or 1.
    pub version: u8,
    /// URI identifying the event scheme (e.g. `"urn:scte:scte35:2014:xml+bin"`).
    pub scheme_id_uri: String,
    /// Optional value qualifying the scheme (empty string when absent).
    pub value: String,
    /// Presentation time in seconds.
    ///
    /// - **Version 0**: `presentation_time_delta / timescale` — offset from the
    ///   segment's earliest presentation time.  Add `sidx.start_s` (when
    ///   available) to convert to an absolute timeline position.
    /// - **Version 1**: `presentation_time / timescale` — absolute position on
    ///   the media presentation timeline.
    pub presentation_time_s: f64,
    /// Duration of the event in seconds.
    ///
    /// [`f64::INFINITY`] when `event_duration == 0xFFFF_FFFF` (indefinite, per
    /// the spec).
    pub duration_s: f64,
    /// Unique identifier for this event within its scheme.
    pub id: u32,
    /// Raw event message payload.
    pub message_data: Bytes,
}

/// Scan `data` for all ISO BMFF `emsg` (DASHEventMessageBox) boxes at the
/// top level and return one [`EmsgInfo`] per box found.
///
/// Returns an empty `Vec` when no `emsg` boxes are present or the buffer is
/// entirely malformed.  Individual boxes that are malformed are silently
/// skipped.
///
/// # Example
///
/// ```rust
/// use dash_rs::mp4::parse_all_emsg;
///
/// // Build a minimal version-1 emsg:
/// //   timescale=1000, presentation_time=5000 (5 s), event_duration=2000 (2 s),
/// //   id=42, scheme_id_uri="urn:example", value="", message_data=b"hello"
/// let mut box_body: Vec<u8> = Vec::new();
/// box_body.push(1);                              // version = 1
/// box_body.extend_from_slice(&[0u8; 3]);        // flags
/// box_body.extend_from_slice(&1000u32.to_be_bytes()); // timescale
/// box_body.extend_from_slice(&5000u64.to_be_bytes()); // presentation_time
/// box_body.extend_from_slice(&2000u32.to_be_bytes()); // event_duration
/// box_body.extend_from_slice(&42u32.to_be_bytes());   // id
/// box_body.extend_from_slice(b"urn:example\0"); // scheme_id_uri
/// box_body.extend_from_slice(b"\0");            // value (empty)
/// box_body.extend_from_slice(b"hello");         // message_data
///
/// let total_size = (8u32 + box_body.len() as u32).to_be_bytes();
/// let mut data: Vec<u8> = total_size.to_vec();
/// data.extend_from_slice(b"emsg");
/// data.extend_from_slice(&box_body);
///
/// let events = parse_all_emsg(&data);
/// assert_eq!(events.len(), 1);
/// assert_eq!(events[0].scheme_id_uri, "urn:example");
/// assert!((events[0].presentation_time_s - 5.0).abs() < 1e-9);
/// assert!((events[0].duration_s - 2.0).abs() < 1e-9);
/// assert_eq!(events[0].id, 42);
/// assert_eq!(&events[0].message_data[..], b"hello");
/// ```
pub fn parse_all_emsg(data: &[u8]) -> Vec<EmsgInfo> {
    let mut result = Vec::new();
    let mut offset = 0usize;

    loop {
        if offset + 8 > data.len() {
            break;
        }

        let raw_size = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        let typ: [u8; 4] = [
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ];

        let (box_size, content_start): (usize, usize) = match raw_size {
            1 => {
                if offset + 16 > data.len() {
                    break;
                }
                let large = u64::from_be_bytes([
                    data[offset + 8],
                    data[offset + 9],
                    data[offset + 10],
                    data[offset + 11],
                    data[offset + 12],
                    data[offset + 13],
                    data[offset + 14],
                    data[offset + 15],
                ]) as usize;
                (large, 16)
            }
            0 => (data.len() - offset, 8),
            n if (n as usize) < 8 => break,
            n => (n as usize, 8),
        };

        let box_end = (offset + box_size).min(data.len());

        if typ == *b"emsg" {
            // content_start is relative to box start; fullbox payload begins there.
            let payload_start = offset + content_start;
            if payload_start < box_end {
                if let Some(emsg) = parse_emsg_fullbox(&data[payload_start..box_end]) {
                    result.push(emsg);
                }
            }
        }

        if offset + box_size > data.len() {
            break;
        }
        offset += box_size;
    }

    result
}

// ── Internal emsg parser ──────────────────────────────────────────────────────

/// Parse the FullBox payload of an `emsg` box.
///
/// `data` must start at the `version` byte (byte 0 = version, bytes 1–3 = flags).
///
/// Version 0 layout (ISO 14496-12 §8.10.2):
/// ```text
/// version(1) flags(3)
/// scheme_id_uri (null-terminated)
/// value         (null-terminated)
/// timescale     (u32)
/// presentation_time_delta (u32)
/// event_duration (u32)
/// id            (u32)
/// message_data  (remaining bytes)
/// ```
///
/// Version 1 layout:
/// ```text
/// version(1) flags(3)
/// timescale (u32)
/// presentation_time (u64)
/// event_duration (u32)
/// id (u32)
/// scheme_id_uri (null-terminated)
/// value         (null-terminated)
/// message_data  (remaining bytes)
/// ```
fn parse_emsg_fullbox(data: &[u8]) -> Option<EmsgInfo> {
    if data.is_empty() {
        return None;
    }
    let version = data[0];
    // flags at [1..4] — not used.

    match version {
        0 => parse_emsg_v0(&data[4..]),
        1 => parse_emsg_v1(&data[4..]),
        _ => None, // unknown version
    }
}

/// Parse the post-flags body of a version-0 `emsg`.
fn parse_emsg_v0(body: &[u8]) -> Option<EmsgInfo> {
    // scheme_id_uri (null-terminated)
    let (scheme_id_uri, rest) = read_cstring(body)?;
    // value (null-terminated)
    let (value, rest) = read_cstring(rest)?;
    // Need at least 16 bytes: timescale(4) + pt_delta(4) + dur(4) + id(4)
    if rest.len() < 16 {
        return None;
    }
    let timescale = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
    if timescale == 0 {
        return None;
    }
    let presentation_time_delta = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]);
    let event_duration = u32::from_be_bytes([rest[8], rest[9], rest[10], rest[11]]);
    let id = u32::from_be_bytes([rest[12], rest[13], rest[14], rest[15]]);
    let message_data = Bytes::copy_from_slice(&rest[16..]);

    let presentation_time_s = presentation_time_delta as f64 / timescale as f64;
    let duration_s = if event_duration == 0xFFFF_FFFF {
        f64::INFINITY
    } else {
        event_duration as f64 / timescale as f64
    };

    Some(EmsgInfo {
        version: 0,
        scheme_id_uri,
        value,
        presentation_time_s,
        duration_s,
        id,
        message_data,
    })
}

/// Parse the post-flags body of a version-1 `emsg`.
fn parse_emsg_v1(body: &[u8]) -> Option<EmsgInfo> {
    // Need at least: timescale(4) + presentation_time(8) + event_duration(4) + id(4) = 20 bytes
    // before the null-terminated strings.
    if body.len() < 20 {
        return None;
    }
    let timescale = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    if timescale == 0 {
        return None;
    }
    let presentation_time = u64::from_be_bytes([
        body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
    ]);
    let event_duration = u32::from_be_bytes([body[12], body[13], body[14], body[15]]);
    let id = u32::from_be_bytes([body[16], body[17], body[18], body[19]]);

    let rest = &body[20..];
    let (scheme_id_uri, rest) = read_cstring(rest)?;
    let (value, rest) = read_cstring(rest)?;
    let message_data = Bytes::copy_from_slice(rest);

    let presentation_time_s = presentation_time as f64 / timescale as f64;
    let duration_s = if event_duration == 0xFFFF_FFFF {
        f64::INFINITY
    } else {
        event_duration as f64 / timescale as f64
    };

    Some(EmsgInfo {
        version: 1,
        scheme_id_uri,
        value,
        presentation_time_s,
        duration_s,
        id,
        message_data,
    })
}

/// Read a null-terminated UTF-8 string from `data`, returning `(string, remainder)`.
/// Returns `None` if no null terminator is found.
fn read_cstring(data: &[u8]) -> Option<(String, &[u8])> {
    let nul = data.iter().position(|&b| b == 0)?;
    let s = String::from_utf8_lossy(&data[..nul]).into_owned();
    Some((s, &data[nul + 1..]))
}

///
/// All times are in **seconds** relative to the stream's presentation
/// timeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SidxInfo {
    /// Presentation time of the first sample in the segment, in seconds.
    pub start_s: f64,
    /// Presentation time of the last sample (exclusive end), in seconds.
    pub end_s: f64,
}

/// Scan `data` for an ISO BMFF `sidx` (Segment Index Box) and, if found,
/// return the segment's presentation-time span as [`SidxInfo`].
///
/// The scan walks the top-level box list from offset 0 and returns the
/// timing derived from the **first** `sidx` encountered.  Returns `None`
/// when:
///
/// - No `sidx` box is present in `data`.
/// - The box is too short to contain the mandatory fields.
/// - `timescale` is zero (would cause a divide-by-zero).
/// - `reference_count` is zero (no segments described).
///
/// # Example
///
/// ```rust
/// use dash_rs::mp4::parse_sidx;
///
/// // Minimal version-0 sidx: timescale=1000, EPT=0, one 3840-tick segment.
/// let box_bytes: &[u8] = &[
///     0, 0, 0, 44,              // size = 44
///     b's', b'i', b'd', b'x',  // type
///     0,                        // version = 0
///     0, 0, 0,                  // flags
///     0, 0, 0, 1,               // reference_id
///     0, 0, 3, 232,             // timescale = 1000
///     0, 0, 0, 0,               // earliest_presentation_time = 0
///     0, 0, 0, 0,               // first_offset = 0
///     0, 0,                     // reserved
///     0, 1,                     // reference_count = 1
///     // reference 0
///     0, 0, 16, 0,              // reference_type + referenced_size
///     0, 0, 15, 0,              // subsegment_duration = 3840
///     128, 0, 0, 0,             // SAP info
/// ];
/// let info = parse_sidx(box_bytes).unwrap();
/// assert!((info.start_s - 0.0).abs() < 1e-9);
/// assert!((info.end_s   - 3.84).abs() < 1e-9);
/// ```
pub fn parse_sidx(data: &[u8]) -> Option<SidxInfo> {
    // find_box returns (slice_from_box_start, fullbox_content_offset).
    // Slicing to [fullbox_content_offset..] gives a slice where byte 0 = version.
    let (box_data, fullbox_start) = find_box(data, *b"sidx")?;
    parse_sidx_fullbox(&box_data[fullbox_start..])
}

/// Presentation-time and byte-range information for a single subsegment entry
/// from a `sidx` box.
///
/// Used by `SegmentBase` addressing to issue exact byte-range `GET` requests
/// for each segment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SidxEntry {
    /// Byte offset of this subsegment from the start of the media file.
    pub byte_offset: u64,
    /// Byte length (referenced_size, lower 31 bits) of this subsegment.
    pub byte_length: u32,
    /// Presentation start time in seconds.
    pub start_s: f64,
    /// Presentation end time (exclusive) in seconds.
    pub end_s: f64,
}

/// Parse a `sidx` box and return one [`SidxEntry`] per reference entry,
/// with byte offsets absolute within the media file.
///
/// `data` is the raw bytes of the range fetched for `SegmentBase@indexRange`
/// (e.g. `Range: bytes=0-999`).  `index_end_offset` is the first byte in the
/// media file that lies **after** the fetched range (i.e.
/// `indexRange_end + 1`).  For the common case where the `indexRange` ends
/// exactly at the last byte of the `sidx` box, the first subsegment begins at
/// `index_end_offset + first_offset` (where `first_offset` is a field inside
/// the `sidx` box itself).
///
/// Returns `None` when the `sidx` is absent, malformed, or has zero entries.
pub fn parse_sidx_entries(data: &[u8], index_end_offset: u64) -> Option<Vec<SidxEntry>> {
    let (box_data, fullbox_start) = find_box(data, *b"sidx")?;
    parse_sidx_entries_fullbox(&box_data[fullbox_start..], index_end_offset)
}

/// Walk the flat top-level box list starting at byte 0 and return
/// `(box_slice, fullbox_offset)` for the first box whose type matches
/// `box_type`, where `box_slice[fullbox_offset]` is the `version` byte of
/// the FullBox header.
///
/// Handles the three legal size encodings from ISO 14496-12 §4.2:
///
/// | `size` field value | Meaning |
/// |---|---|
/// | `>= 8`             | Normal: box occupies exactly `size` bytes |
/// | `1`                | Large-size: the next 8 bytes contain a `u64` size |
/// | `0`                | Box extends to the end of the stream / buffer |
///
/// Returns `None` when the requested type is not found or the buffer is
/// truncated.
fn find_box(data: &[u8], box_type: [u8; 4]) -> Option<(&[u8], usize)> {
    let mut offset = 0usize;

    loop {
        // Need at least a standard 8-byte header (size + type).
        if offset + 8 > data.len() {
            return None;
        }

        let raw_size = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        let typ: [u8; 4] = [
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ];

        // Determine full box size and where the FullBox content begins.
        // For a FullBox the content starts right after (size + type) or
        // after (size=1 marker + type + largesize).
        let (box_size, fullbox_content_offset): (usize, usize) = match raw_size {
            1 => {
                // Large-size encoding: 8-byte size follows the type.
                if offset + 16 > data.len() {
                    return None; // truncated
                }
                let large = u64::from_be_bytes([
                    data[offset + 8],
                    data[offset + 9],
                    data[offset + 10],
                    data[offset + 11],
                    data[offset + 12],
                    data[offset + 13],
                    data[offset + 14],
                    data[offset + 15],
                ]) as usize;
                // FullBox content starts after: 4 (size=1) + 4 (type) + 8 (largesize) = 16.
                (large, 16)
            }
            0 => {
                // Box extends to the end of the buffer.
                // FullBox content starts at offset 8.
                (data.len() - offset, 8)
            }
            n if (n as usize) < 8 => {
                // Malformed: size must be at least 8.
                return None;
            }
            n => (n as usize, 8),
        };

        if typ == box_type {
            let end = (offset + box_size).min(data.len());
            return Some((&data[offset..end], fullbox_content_offset));
        }

        // Advance past this box.  Guard against truncation / infinite loop.
        if offset + box_size > data.len() {
            return None;
        }
        offset += box_size;
    }
}

/// Parse the FullBox payload of a `sidx` box.
///
/// `data` must start at the `version` byte (offset 0 = version).
///
/// FullBox layout relative to the `version` byte (ISO 14496-12 §8.16.3):
///
/// ```text
/// Offset  Size  Field
///  0       1    version
///  1       3    flags
///  4       4    reference_ID
///  8       4    timescale
///
/// version == 0:
/// 12       4    earliest_presentation_time  (u32)
/// 16       4    first_offset                (u32)
/// 20       2    reserved
/// 22       2    reference_count
/// 24       …    references  (12 bytes each)
///
/// version == 1:
/// 12       8    earliest_presentation_time  (u64)
/// 20       8    first_offset                (u64)
/// 28       2    reserved
/// 30       2    reference_count
/// 32       …    references  (12 bytes each)
///
/// Each reference (12 bytes):
///   0–3   reference_type (1 bit) + referenced_size (31 bits)
///   4–7   subsegment_duration  (u32)
///   8–11  starts_with_SAP (1 bit) + SAP_type (3 bits) + SAP_delta_time (28 bits)
/// ```
fn parse_sidx_fullbox(data: &[u8]) -> Option<SidxInfo> {
    // Minimum size for version 0 with 0 references: 24 bytes.
    if data.len() < 24 {
        return None;
    }

    let version = data[0];
    // flags at [1..4] — not needed for timing.
    // reference_ID at [4..8] — not needed for timing.

    let timescale = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    if timescale == 0 {
        return None;
    }

    let (earliest_presentation_time, refs_start): (u64, usize) = if version == 0 {
        let ept = u32::from_be_bytes([data[12], data[13], data[14], data[15]]) as u64;
        // first_offset at [16..20], reserved at [20..22], ref_count at [22..24].
        (ept, 24)
    } else {
        // version 1 needs 8 more bytes for u64 EPT and u64 first_offset.
        if data.len() < 32 {
            return None;
        }
        let ept = u64::from_be_bytes([
            data[12], data[13], data[14], data[15], data[16], data[17], data[18], data[19],
        ]);
        // first_offset at [20..28], reserved at [28..30], ref_count at [30..32].
        (ept, 32)
    };

    // reference_count is 2 bytes immediately before refs_start.
    let rc_off = refs_start - 2;
    let reference_count = u16::from_be_bytes([data[rc_off], data[rc_off + 1]]) as usize;
    if reference_count == 0 {
        return None;
    }

    // Guard: verify the buffer holds all reference entries.
    let refs_end = refs_start.checked_add(reference_count.checked_mul(12)?)?;
    if data.len() < refs_end {
        return None;
    }

    // Sum `subsegment_duration` fields (bytes 4–7 of each 12-byte entry).
    let mut total_ticks: u64 = 0;
    for i in 0..reference_count {
        let off = refs_start + i * 12;
        let dur =
            u32::from_be_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]]) as u64;
        total_ticks = total_ticks.saturating_add(dur);
    }

    let ts = timescale as f64;
    Some(SidxInfo {
        start_s: earliest_presentation_time as f64 / ts,
        end_s: (earliest_presentation_time + total_ticks) as f64 / ts,
    })
}

/// Parse the FullBox payload of a `sidx` box and return per-entry byte-range
/// and timing information for `SegmentBase` addressing.
///
/// `data` starts at the `version` byte (same contract as
/// [`parse_sidx_fullbox`]).  `index_end_offset` is the first byte in the
/// media file that lies after the last byte of the fetched index range.
fn parse_sidx_entries_fullbox(data: &[u8], index_end_offset: u64) -> Option<Vec<SidxEntry>> {
    if data.len() < 24 {
        return None;
    }

    let version = data[0];
    let timescale = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    if timescale == 0 {
        return None;
    }

    let (earliest_presentation_time, first_offset, refs_start): (u64, u64, usize) = if version == 0
    {
        let ept = u32::from_be_bytes([data[12], data[13], data[14], data[15]]) as u64;
        let fo = u32::from_be_bytes([data[16], data[17], data[18], data[19]]) as u64;
        (ept, fo, 24)
    } else {
        if data.len() < 32 {
            return None;
        }
        let ept = u64::from_be_bytes([
            data[12], data[13], data[14], data[15], data[16], data[17], data[18], data[19],
        ]);
        let fo = u64::from_be_bytes([
            data[20], data[21], data[22], data[23], data[24], data[25], data[26], data[27],
        ]);
        (ept, fo, 32)
    };

    let rc_off = refs_start - 2;
    let reference_count = u16::from_be_bytes([data[rc_off], data[rc_off + 1]]) as usize;
    if reference_count == 0 {
        return None;
    }

    let refs_end = refs_start.checked_add(reference_count.checked_mul(12)?)?;
    if data.len() < refs_end {
        return None;
    }

    let ts = timescale as f64;
    let mut entries = Vec::with_capacity(reference_count);
    let mut current_byte: u64 = index_end_offset + first_offset;
    let mut current_time: u64 = earliest_presentation_time;

    for i in 0..reference_count {
        let off = refs_start + i * 12;
        // reference_type (1 bit) + referenced_size (31 bits).
        let word0 = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        let referenced_size = word0 & 0x7FFF_FFFF;
        let subseg_dur =
            u32::from_be_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]]) as u64;

        entries.push(SidxEntry {
            byte_offset: current_byte,
            byte_length: referenced_size,
            start_s: current_time as f64 / ts,
            end_s: (current_time + subseg_dur) as f64 / ts,
        });

        current_byte += referenced_size as u64;
        current_time += subseg_dur;
    }

    Some(entries)
}

// ── ProducerReferenceTime / parse_prft ───────────────────────────────────────

/// Seconds between the NTP epoch (1900-01-01 00:00:00 UTC) and the Unix epoch
/// (1970-01-01 00:00:00 UTC), accounting for 17 leap years in the 70-year span.
const NTP_UNIX_EPOCH_DELTA_S: f64 = 2_208_988_800.0;

/// Decoded data from a `ProducerReferenceTimeBox` (`prft`, ISO 14496-12 §8.10.5).
///
/// The `prft` box maps a media timeline position to a wall-clock producer
/// time, enabling accurate end-to-end latency measurement for low-latency
/// live streams.
///
/// # Wall-clock conversion
///
/// `ntp_timestamp` is a 64-bit NTP fixed-point value:
/// - Upper 32 bits: seconds since 1900-01-01 00:00:00 UTC.
/// - Lower 32 bits: sub-second fraction (1/2³² seconds per tick).
///
/// `wall_clock_s` is the pre-computed Unix-epoch equivalent:
/// ```text
/// wall_clock_s = (ntp_timestamp >> 32) + (ntp_timestamp & 0xFFFF_FFFF) / 2³² − NTP_DELTA
/// ```
///
/// # Media time
///
/// `media_time` is in the timescale ticks of the track identified by
/// `reference_track_id`.  Divide by the track's known timescale to obtain
/// the equivalent presentation-timeline position in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct ProducerReferenceTime {
    /// Box version: `0` (32-bit `media_time`) or `1` (64-bit `media_time`).
    pub version: u8,
    /// ID of the track whose timescale `media_time` is expressed in.
    pub reference_track_id: u32,
    /// Raw NTP 64-bit fixed-point timestamp.
    ///
    /// Upper 32 bits = seconds since 1900-01-01; lower 32 bits = fractional.
    pub ntp_timestamp: u64,
    /// Producer wall-clock time as a Unix timestamp in seconds.
    ///
    /// Pre-computed from `ntp_timestamp` by subtracting the NTP-to-Unix delta
    /// (2 208 988 800 s) and adding the fractional part.
    pub wall_clock_s: f64,
    /// Media timeline position in timescale ticks of `reference_track_id`.
    ///
    /// Divide by the track's timescale to get the equivalent seconds.
    pub media_time: u64,
}

/// Scan `data` for the first ISO BMFF `prft` (ProducerReferenceTimeBox) box
/// and decode it.
///
/// Returns `None` when no `prft` box is present or the box is malformed.
///
/// # Wire format (FullBox)
///
/// ```text
/// version(1) + flags(3) + reference_track_id(4) + ntp_timestamp(8)
/// + media_time(4 if version==0, 8 if version==1)
/// ```
///
/// Minimum body size: 20 bytes (version 0) or 24 bytes (version 1).
pub fn parse_prft(data: &[u8]) -> Option<ProducerReferenceTime> {
    let (box_data, fullbox_start) = find_box(data, *b"prft")?;
    let payload = &box_data[fullbox_start..];
    // FullBox: version(1) + flags(3) = 4 bytes.
    if payload.len() < 4 {
        return None;
    }
    let version = payload[0];
    // Minimum payload after version+flags: ref_id(4) + ntp(8) + media_time(4 or 8).
    let min_len = if version == 0 {
        4 + 4 + 8 + 4
    } else {
        4 + 4 + 8 + 8
    };
    if payload.len() < min_len {
        return None;
    }
    let reference_track_id = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let ntp_timestamp = u64::from_be_bytes([
        payload[8],
        payload[9],
        payload[10],
        payload[11],
        payload[12],
        payload[13],
        payload[14],
        payload[15],
    ]);
    let media_time = if version == 0 {
        u32::from_be_bytes([payload[16], payload[17], payload[18], payload[19]]) as u64
    } else {
        u64::from_be_bytes([
            payload[16],
            payload[17],
            payload[18],
            payload[19],
            payload[20],
            payload[21],
            payload[22],
            payload[23],
        ])
    };

    // Convert NTP timestamp to Unix epoch seconds.
    let ntp_secs = (ntp_timestamp >> 32) as f64;
    let ntp_frac = (ntp_timestamp & 0xFFFF_FFFF) as f64 / 4_294_967_296.0; // 2^32
    let wall_clock_s = ntp_secs + ntp_frac - NTP_UNIX_EPOCH_DELTA_S;

    Some(ProducerReferenceTime {
        version,
        reference_track_id,
        ntp_timestamp,
        wall_clock_s,
        media_time,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal version-0 sidx box.
    ///
    /// Fixed-field layout:
    ///   size(4) type(4) version(1) flags(3) ref_id(4) timescale(4)
    ///   ept(4) first_offset(4) reserved(2) ref_count(2)
    ///   [ ref_type_and_size(4) duration(4) sap(4) ] × ref_count
    fn make_sidx_v0(timescale: u32, ept: u32, durations: &[u32]) -> Vec<u8> {
        let ref_count = durations.len() as u16;
        let size: u32 = 32 + 12 * ref_count as u32;
        let mut buf = Vec::with_capacity(size as usize);

        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"sidx");
        buf.push(0); // version
        buf.extend_from_slice(&[0u8; 3]); // flags
        buf.extend_from_slice(&1u32.to_be_bytes()); // reference_id
        buf.extend_from_slice(&timescale.to_be_bytes());
        buf.extend_from_slice(&ept.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // first_offset
        buf.extend_from_slice(&[0u8; 2]); // reserved
        buf.extend_from_slice(&ref_count.to_be_bytes());

        for &dur in durations {
            buf.extend_from_slice(&0x0000_1000u32.to_be_bytes()); // ref type + size
            buf.extend_from_slice(&dur.to_be_bytes());
            buf.extend_from_slice(&0x8000_0000u32.to_be_bytes()); // SAP
        }
        buf
    }

    /// Build a minimal version-1 sidx box (u64 EPT and first_offset).
    fn make_sidx_v1(timescale: u32, ept: u64, durations: &[u32]) -> Vec<u8> {
        let ref_count = durations.len() as u16;
        let size: u32 = 40 + 12 * ref_count as u32;
        let mut buf = Vec::with_capacity(size as usize);

        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(b"sidx");
        buf.push(1); // version
        buf.extend_from_slice(&[0u8; 3]); // flags
        buf.extend_from_slice(&1u32.to_be_bytes()); // reference_id
        buf.extend_from_slice(&timescale.to_be_bytes());
        buf.extend_from_slice(&ept.to_be_bytes());
        buf.extend_from_slice(&0u64.to_be_bytes()); // first_offset
        buf.extend_from_slice(&[0u8; 2]); // reserved
        buf.extend_from_slice(&ref_count.to_be_bytes());

        for &dur in durations {
            buf.extend_from_slice(&0x0000_1000u32.to_be_bytes());
            buf.extend_from_slice(&dur.to_be_bytes());
            buf.extend_from_slice(&0x8000_0000u32.to_be_bytes());
        }
        buf
    }

    // ── correctness ───────────────────────────────────────────────────────────

    #[test]
    fn v0_single_reference_from_zero() {
        // timescale=1000, EPT=0, duration=3840 → [0.0, 3.84)
        let info = parse_sidx(&make_sidx_v0(1000, 0, &[3840])).unwrap();
        assert!((info.start_s - 0.0).abs() < 1e-9, "start={}", info.start_s);
        assert!((info.end_s - 3.84).abs() < 1e-9, "end={}", info.end_s);
    }

    #[test]
    fn v0_nonzero_ept() {
        // EPT=1000 (1 s), duration=2000 (2 s) → [1.0, 3.0)
        let info = parse_sidx(&make_sidx_v0(1000, 1000, &[2000])).unwrap();
        assert!((info.start_s - 1.0).abs() < 1e-9, "start={}", info.start_s);
        assert!((info.end_s - 3.0).abs() < 1e-9, "end={}", info.end_s);
    }

    #[test]
    fn v0_multiple_references_summed() {
        // Two sub-segments: 1920 + 1920 = 3840 ticks at ts=1000 → 3.84 s
        let info = parse_sidx(&make_sidx_v0(1000, 0, &[1920, 1920])).unwrap();
        assert!((info.start_s - 0.0).abs() < 1e-9);
        assert!((info.end_s - 3.84).abs() < 1e-9, "end={}", info.end_s);
    }

    #[test]
    fn v0_high_timescale() {
        // timescale=90000 (typical H.264), EPT=180000 (2 s), duration=360000 (4 s)
        let info = parse_sidx(&make_sidx_v0(90_000, 180_000, &[360_000])).unwrap();
        assert!((info.start_s - 2.0).abs() < 1e-9, "start={}", info.start_s);
        assert!((info.end_s - 6.0).abs() < 1e-9, "end={}", info.end_s);
    }

    #[test]
    fn v1_large_ept() {
        // version=1, EPT=10_000_000_000 (large u64), duration=4000
        let info = parse_sidx(&make_sidx_v1(1000, 10_000_000_000u64, &[4000])).unwrap();
        let expected_start = 10_000_000_000.0_f64 / 1000.0;
        assert!(
            (info.start_s - expected_start).abs() < 1e-3,
            "start={}",
            info.start_s
        );
        assert!(
            (info.end_s - (expected_start + 4.0)).abs() < 1e-3,
            "end={}",
            info.end_s
        );
    }

    #[test]
    fn box_preceded_by_styp() {
        // Real segments often begin with a styp box before sidx.
        let mut data: Vec<u8> = vec![
            0, 0, 0, 20, // size = 20
            b's', b't', b'y', b'p', // type
            b'd', b'a', b's', b'h', // major brand
            0, 0, 0, 0, // minor version
            b'd', b'a', b's', b'h', // compatible brand
        ];
        data.extend_from_slice(&make_sidx_v0(1000, 500, &[2500]));

        let info = parse_sidx(&data).unwrap();
        assert!((info.start_s - 0.5).abs() < 1e-9, "start={}", info.start_s);
        assert!((info.end_s - 3.0).abs() < 1e-9, "end={}", info.end_s);
    }

    #[test]
    fn large_size_box_correctly_parsed() {
        // Construct a sidx using size==1 / largesize encoding and verify it
        // is parsed correctly.
        //
        // Large-size box layout:
        //   [0..4]   size = 1 (marker)
        //   [4..8]   type = 'sidx'
        //   [8..16]  largesize (u64) = total box size
        //   [16..]   FullBox content (version, flags, ...)
        //
        // We manually build the payload (version onwards) with:
        //   timescale=1000, EPT=0, 1 reference of duration=3840.
        let version: u8 = 0;
        let flags = [0u8; 3];
        let ref_id = 1u32;
        let timescale = 1000u32;
        let ept = 0u32;
        let first_offset = 0u32;
        let ref_count = 1u16;
        let ref_type_size = 0x0000_1000u32;
        let duration = 3840u32;
        let sap = 0x8000_0000u32;

        // Payload = everything after the box header (version onwards).
        let mut payload: Vec<u8> = Vec::new();
        payload.push(version);
        payload.extend_from_slice(&flags);
        payload.extend_from_slice(&ref_id.to_be_bytes());
        payload.extend_from_slice(&timescale.to_be_bytes());
        payload.extend_from_slice(&ept.to_be_bytes());
        payload.extend_from_slice(&first_offset.to_be_bytes());
        payload.extend_from_slice(&[0u8; 2]); // reserved
        payload.extend_from_slice(&ref_count.to_be_bytes());
        payload.extend_from_slice(&ref_type_size.to_be_bytes());
        payload.extend_from_slice(&duration.to_be_bytes());
        payload.extend_from_slice(&sap.to_be_bytes());

        // Total box size: 4 (size=1) + 4 (type) + 8 (largesize) + payload.len()
        let total_size: u64 = (4 + 4 + 8 + payload.len()) as u64;

        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes()); // size = 1
        data.extend_from_slice(b"sidx");
        data.extend_from_slice(&total_size.to_be_bytes());
        data.extend_from_slice(&payload);

        let info = parse_sidx(&data).unwrap();
        assert!((info.start_s - 0.0).abs() < 1e-9, "start={}", info.start_s);
        assert!((info.end_s - 3.84).abs() < 1e-9, "end={}", info.end_s);
    }

    // ── edge cases / error paths ───────────────────────────────────────────────

    #[test]
    fn no_sidx_returns_none() {
        // Buffer contains only a moof box — no sidx.
        assert!(parse_sidx(&[0, 0, 0, 8, b'm', b'o', b'o', b'f']).is_none());
    }

    #[test]
    fn empty_data_returns_none() {
        assert!(parse_sidx(&[]).is_none());
    }

    #[test]
    fn truncated_box_returns_none() {
        // Box claims size=44 but buffer only contains 20 bytes.
        let data: &[u8] = &[
            0, 0, 0, 44, b's', b'i', b'd', b'x', 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 3, 232,
        ];
        assert!(parse_sidx(data).is_none());
    }

    #[test]
    fn zero_timescale_returns_none() {
        let mut data = make_sidx_v0(1000, 0, &[1000]);
        // Overwrite timescale at fullbox offset 8 → absolute offset 8+8 = 16.
        data[16] = 0;
        data[17] = 0;
        data[18] = 0;
        data[19] = 0;
        assert!(parse_sidx(&data).is_none());
    }

    #[test]
    fn zero_reference_count_returns_none() {
        let mut data = make_sidx_v0(1000, 0, &[1000]);
        // ref_count is at fullbox offset 22 → absolute offset 8+22 = 30.
        data[30] = 0;
        data[31] = 0;
        assert!(parse_sidx(&data).is_none());
    }

    #[test]
    fn box_size_zero_extends_to_eof() {
        // Build a sidx whose size field is 0 (extends to EOF).
        let mut data = make_sidx_v0(1000, 0, &[3840]);
        // Overwrite the size field with 0.
        data[0] = 0;
        data[1] = 0;
        data[2] = 0;
        data[3] = 0;
        // The box should still be found and parsed correctly.
        let info = parse_sidx(&data).unwrap();
        assert!((info.start_s - 0.0).abs() < 1e-9);
        assert!((info.end_s - 3.84).abs() < 1e-9);
    }

    // ── emsg helpers ──────────────────────────────────────────────────────────

    /// Build a version-0 `emsg` box.
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
        body.push(0); // null terminator
        body.extend_from_slice(value.as_bytes());
        body.push(0); // null terminator
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

    /// Build a version-1 `emsg` box.
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
        body.push(0); // null terminator
        body.extend_from_slice(value.as_bytes());
        body.push(0); // null terminator
        body.extend_from_slice(message_data);

        let size = (8u32 + body.len() as u32).to_be_bytes();
        let mut buf = size.to_vec();
        buf.extend_from_slice(b"emsg");
        buf.extend_from_slice(&body);
        buf
    }

    // ── emsg correctness ──────────────────────────────────────────────────────

    #[test]
    fn emsg_v0_basic_fields() {
        let data = make_emsg_v0(
            "urn:scte:scte35:2014:xml+bin",
            "1",
            90_000,
            180_000,
            270_000,
            7,
            b"payload",
        );
        let events = parse_all_emsg(&data);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.version, 0);
        assert_eq!(e.scheme_id_uri, "urn:scte:scte35:2014:xml+bin");
        assert_eq!(e.value, "1");
        // presentation_time_delta=180000, timescale=90000 → 2.0 s
        assert!(
            (e.presentation_time_s - 2.0).abs() < 1e-9,
            "pt={}",
            e.presentation_time_s
        );
        // event_duration=270000, timescale=90000 → 3.0 s
        assert!((e.duration_s - 3.0).abs() < 1e-9, "dur={}", e.duration_s);
        assert_eq!(e.id, 7);
        assert_eq!(&e.message_data[..], b"payload");
    }

    #[test]
    fn emsg_v1_basic_fields() {
        // timescale=1000, presentation_time=5000 → 5 s; duration=2000 → 2 s
        let data = make_emsg_v1("urn:example:test", "val", 1000, 5000, 2000, 42, b"hello");
        let events = parse_all_emsg(&data);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.version, 1);
        assert_eq!(e.scheme_id_uri, "urn:example:test");
        assert_eq!(e.value, "val");
        assert!(
            (e.presentation_time_s - 5.0).abs() < 1e-9,
            "pt={}",
            e.presentation_time_s
        );
        assert!((e.duration_s - 2.0).abs() < 1e-9, "dur={}", e.duration_s);
        assert_eq!(e.id, 42);
        assert_eq!(&e.message_data[..], b"hello");
    }

    #[test]
    fn emsg_indefinite_duration_v0() {
        let data = make_emsg_v0("urn:x", "", 1000, 0, 0xFFFF_FFFF, 1, b"");
        let events = parse_all_emsg(&data);
        assert_eq!(events.len(), 1);
        assert!(
            events[0].duration_s.is_infinite(),
            "expected INFINITY for 0xFFFFFFFF event_duration"
        );
    }

    #[test]
    fn emsg_indefinite_duration_v1() {
        let data = make_emsg_v1("urn:x", "", 1000, 0, 0xFFFF_FFFF, 1, b"");
        let events = parse_all_emsg(&data);
        assert_eq!(events.len(), 1);
        assert!(events[0].duration_s.is_infinite());
    }

    #[test]
    fn emsg_empty_value_string() {
        // Both scheme_id_uri and value can be empty.
        let data = make_emsg_v1("", "", 1000, 0, 1000, 0, b"");
        let events = parse_all_emsg(&data);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scheme_id_uri, "");
        assert_eq!(events[0].value, "");
    }

    #[test]
    fn emsg_multiple_boxes_in_segment() {
        // Two emsg boxes back-to-back; both should be returned.
        let mut data = make_emsg_v1("urn:a", "", 1000, 1000, 500, 1, b"first");
        data.extend_from_slice(&make_emsg_v1("urn:b", "", 1000, 2000, 500, 2, b"second"));
        let events = parse_all_emsg(&data);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].scheme_id_uri, "urn:a");
        assert_eq!(&events[0].message_data[..], b"first");
        assert_eq!(events[1].scheme_id_uri, "urn:b");
        assert_eq!(&events[1].message_data[..], b"second");
    }

    #[test]
    fn emsg_mixed_with_other_boxes() {
        // Real segments: styp + sidx + emsg.  Both sidx and emsg should parse.
        let styp: &[u8] = &[
            0, 0, 0, 20, b's', b't', b'y', b'p', b'd', b'a', b's', b'h', 0, 0, 0, 0, b'd', b'a',
            b's', b'h',
        ];
        let sidx = make_sidx_v0(1000, 0, &[2000]);
        let emsg = make_emsg_v1("urn:test", "", 1000, 500, 1000, 9, b"data");

        let mut data = styp.to_vec();
        data.extend_from_slice(&sidx);
        data.extend_from_slice(&emsg);

        // sidx still found
        let info = parse_sidx(&data).unwrap();
        assert!((info.start_s - 0.0).abs() < 1e-9);
        assert!((info.end_s - 2.0).abs() < 1e-9);

        // emsg still found
        let events = parse_all_emsg(&data);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scheme_id_uri, "urn:test");
        assert_eq!(events[0].id, 9);
    }

    #[test]
    fn emsg_no_emsg_returns_empty() {
        // A buffer with only a sidx: parse_all_emsg should return empty.
        let data = make_sidx_v0(1000, 0, &[1000]);
        assert!(parse_all_emsg(&data).is_empty());
    }

    #[test]
    fn emsg_empty_buffer_returns_empty() {
        assert!(parse_all_emsg(&[]).is_empty());
    }

    #[test]
    fn emsg_zero_timescale_skipped() {
        // A v1 emsg with timescale=0 must be skipped (would cause divide-by-zero).
        let data = make_emsg_v1("urn:x", "", 0, 0, 0, 0, b"");
        // parse_all_emsg silently skips malformed boxes.
        assert!(parse_all_emsg(&data).is_empty());
    }

    #[test]
    fn emsg_large_message_payload() {
        // Verify that large message_data round-trips correctly.
        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let data = make_emsg_v1("urn:big", "0", 90_000, 0, 90_000, 0, &payload);
        let events = parse_all_emsg(&data);
        assert_eq!(events.len(), 1);
        assert_eq!(&events[0].message_data[..], payload.as_slice());
    }

    // ── prft tests ────────────────────────────────────────────────────────────

    /// Build a minimal version-0 `prft` box.
    ///
    /// Layout (fullbox payload, after standard 8-byte header):
    ///   version(1) flags(3) reference_track_id(4) ntp_timestamp(8) media_time(4)
    fn make_prft_v0(reference_track_id: u32, ntp_timestamp: u64, media_time: u32) -> Vec<u8> {
        // Body size: 4 (version+flags) + 4 (ref_id) + 8 (ntp) + 4 (media_time) = 20
        let body_size: u32 = 20;
        let total_size: u32 = 8 + body_size;
        let mut buf = Vec::with_capacity(total_size as usize);
        buf.extend_from_slice(&total_size.to_be_bytes());
        buf.extend_from_slice(b"prft");
        buf.push(0); // version = 0
        buf.extend_from_slice(&[0u8; 3]); // flags
        buf.extend_from_slice(&reference_track_id.to_be_bytes());
        buf.extend_from_slice(&ntp_timestamp.to_be_bytes());
        buf.extend_from_slice(&media_time.to_be_bytes());
        buf
    }

    /// Build a minimal version-1 `prft` box (64-bit media_time).
    fn make_prft_v1(reference_track_id: u32, ntp_timestamp: u64, media_time: u64) -> Vec<u8> {
        // Body: 4 + 4 + 8 + 8 = 24
        let body_size: u32 = 24;
        let total_size: u32 = 8 + body_size;
        let mut buf = Vec::with_capacity(total_size as usize);
        buf.extend_from_slice(&total_size.to_be_bytes());
        buf.extend_from_slice(b"prft");
        buf.push(1); // version = 1
        buf.extend_from_slice(&[0u8; 3]); // flags
        buf.extend_from_slice(&reference_track_id.to_be_bytes());
        buf.extend_from_slice(&ntp_timestamp.to_be_bytes());
        buf.extend_from_slice(&media_time.to_be_bytes());
        buf
    }

    #[test]
    fn prft_v0_parses_correctly() {
        // NTP timestamp for 2024-01-01 00:00:00 UTC:
        // Unix: 1704067200 s → NTP: 1704067200 + 2208988800 = 3913056000 s
        // As 64-bit fixed-point: 3913056000u64 << 32 (no fractional part)
        let ntp_secs: u64 = 3_913_056_000;
        let ntp_ts = ntp_secs << 32; // no fractional part
        let data = make_prft_v0(1, ntp_ts, 48_000);
        let prft = parse_prft(&data).expect("should parse v0 prft");
        assert_eq!(prft.version, 0);
        assert_eq!(prft.reference_track_id, 1);
        assert_eq!(prft.ntp_timestamp, ntp_ts);
        assert_eq!(prft.media_time, 48_000);
        // wall_clock_s = 3913056000 - 2208988800 = 1704067200
        assert!(
            (prft.wall_clock_s - 1_704_067_200.0).abs() < 1.0,
            "wall_clock_s = {}",
            prft.wall_clock_s
        );
    }

    #[test]
    fn prft_v1_parses_64bit_media_time() {
        let ntp_secs: u64 = 3_913_056_000;
        let ntp_ts = ntp_secs << 32;
        // Large 64-bit media_time that would overflow u32.
        let media_time: u64 = 0x1_0000_0000_u64;
        let data = make_prft_v1(2, ntp_ts, media_time);
        let prft = parse_prft(&data).expect("should parse v1 prft");
        assert_eq!(prft.version, 1);
        assert_eq!(prft.reference_track_id, 2);
        assert_eq!(prft.media_time, media_time);
    }

    #[test]
    fn prft_fractional_ntp_converts_correctly() {
        // NTP timestamp with 0.5 s fractional part:
        // ntp_fraction = 0x8000_0000 ≈ 2^31 / 2^32 = 0.5
        let ntp_secs: u64 = 3_913_056_000;
        let ntp_ts = (ntp_secs << 32) | 0x8000_0000;
        let data = make_prft_v0(1, ntp_ts, 0);
        let prft = parse_prft(&data).unwrap();
        // wall_clock_s should be 1704067200.5 approximately
        assert!(
            (prft.wall_clock_s - 1_704_067_200.5).abs() < 1e-6,
            "wall_clock_s = {}",
            prft.wall_clock_s
        );
    }

    #[test]
    fn prft_not_present_returns_none() {
        // A buffer with only a sidx: parse_prft should return None.
        let data = make_sidx_v0(1000, 0, &[1000]);
        assert!(parse_prft(&data).is_none());
    }

    #[test]
    fn prft_truncated_box_returns_none() {
        // Build a valid v0 prft then truncate it.
        let ntp_ts = 3_913_056_000u64 << 32;
        let mut data = make_prft_v0(1, ntp_ts, 0);
        data.truncate(data.len() - 5); // chop off 5 bytes
        assert!(parse_prft(&data).is_none());
    }

    #[test]
    fn prft_mixed_with_other_boxes() {
        // sidx before prft — both should be parseable.
        let sidx = make_sidx_v0(1000, 0, &[2000]);
        let ntp_ts = 3_913_056_000u64 << 32;
        let prft_box = make_prft_v0(1, ntp_ts, 96_000);
        let mut data = sidx.clone();
        data.extend_from_slice(&prft_box);

        let sidx_info = parse_sidx(&data).unwrap();
        assert!((sidx_info.start_s).abs() < 1e-9);

        let prft = parse_prft(&data).unwrap();
        assert_eq!(prft.reference_track_id, 1);
        assert_eq!(prft.media_time, 96_000);
    }

    // ── parse_sidx_entries ────────────────────────────────────────────────────

    /// `index_end_offset` is the byte immediately after the fetched index range.
    /// With `first_offset = 0` in the sidx, the first entry starts there.
    #[test]
    fn sidx_entries_v0_single_entry_byte_offsets() {
        // timescale=1000, EPT=0, one entry of duration=3840.
        // The test sidx helper sets referenced_size = 0x1000 = 4096.
        let data = make_sidx_v0(1000, 0, &[3840]);
        let index_end: u64 = 5000; // arbitrary file offset
        let entries = parse_sidx_entries(&data, index_end).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].byte_offset, index_end); // first_offset = 0
        assert_eq!(entries[0].byte_length, 0x1000); // 4096 as built by helper
        assert!((entries[0].start_s - 0.0).abs() < 1e-9);
        assert!((entries[0].end_s - 3.84).abs() < 1e-9);
    }

    #[test]
    fn sidx_entries_v0_multiple_entries_cumulative_offsets() {
        // Two entries; referenced_size = 4096 each (helper always uses 0x1000).
        let data = make_sidx_v0(1000, 0, &[2000, 1840]);
        let index_end: u64 = 1000;
        let entries = parse_sidx_entries(&data, index_end).unwrap();

        assert_eq!(entries.len(), 2);
        // Entry 0 starts at index_end.
        assert_eq!(entries[0].byte_offset, 1000);
        // Entry 1 starts at index_end + referenced_size_0.
        assert_eq!(entries[1].byte_offset, 1000 + 4096);

        // Timing: timescale=1000, EPT=0.
        assert!((entries[0].start_s - 0.0).abs() < 1e-9);
        assert!((entries[0].end_s - 2.0).abs() < 1e-9);
        assert!((entries[1].start_s - 2.0).abs() < 1e-9);
        assert!((entries[1].end_s - 3.84).abs() < 1e-9);
    }

    #[test]
    fn sidx_entries_v1_large_first_offset() {
        // version-1 sidx with a large EPT and two entries.
        let data = make_sidx_v1(1000, 10_000_000_000u64, &[4000]);
        let index_end: u64 = 8192;
        let entries = parse_sidx_entries(&data, index_end).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].byte_offset, index_end); // first_offset=0 in helper
        let expected_start = 10_000_000_000.0_f64 / 1000.0;
        assert!((entries[0].start_s - expected_start).abs() < 0.01);
        assert!((entries[0].end_s - (expected_start + 4.0)).abs() < 0.01);
    }

    #[test]
    fn sidx_entries_zero_ref_count_returns_none() {
        // A sidx with reference_count=0 should return None.
        let data = make_sidx_v0(1000, 0, &[]);
        assert!(parse_sidx_entries(&data, 0).is_none());
    }

    #[test]
    fn sidx_entries_truncated_returns_none() {
        // A sidx buffer truncated before all entries are readable → None.
        let data = make_sidx_v0(1000, 0, &[3840]);
        // Drop the last 4 bytes so the single entry cannot be fully parsed.
        let truncated = &data[..data.len() - 4];
        assert!(parse_sidx_entries(truncated, 0).is_none());
    }

    // ── parse_prft — version handling ────────────────────────────────────────

    /// version=2 is parsed like version=1 (64-bit media_time path) because the
    /// implementation treats any non-zero version as version-1.  This test
    /// documents that known behaviour.
    #[test]
    fn prft_non_zero_version_parsed_as_v1_variant() {
        // Build a v0-shaped box but set version byte = 2.
        let mut data = make_prft_v0(1, 3_913_056_000u64 << 32, 0);
        // Version byte is at offset 8 (size[4] + type[4]).
        data[8] = 2;
        // With version=2 the code takes the 64-bit media_time path.
        // The v0 helper only writes 4 bytes of media_time → payload too short → None.
        assert!(
            parse_prft(&data).is_none(),
            "version=2 with a v0-sized box should fail because payload is too short"
        );
    }

    // ── parse_all_emsg — v0 zero-timescale skipped ──────────────────────────

    #[test]
    fn emsg_v0_zero_timescale_skipped() {
        // Build a v0 emsg with timescale=0; the parser should skip it.
        let mut data = make_emsg_v0(
            "urn:test", "value", 0, // timescale = 0 ← should be skipped
            0, 1000, 0, b"",
        );
        // The `make_emsg_v0` helper already sets timescale=0 above;
        // just verify the result.
        assert!(
            parse_all_emsg(&data).is_empty(),
            "emsg with timescale=0 should be skipped"
        );
        // Sanity: with a valid non-zero timescale the same scheme parses fine.
        data = make_emsg_v0("urn:test", "val", 90_000, 0, 1000, 0, b"");
        assert_eq!(parse_all_emsg(&data).len(), 1);
    }
}
