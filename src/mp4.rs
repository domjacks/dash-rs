//! Minimal ISO Base Media File Format (ISOBMFF) box parsing.
//!
//! This module implements only the subset of the spec needed to extract
//! presentation-time information from MPEG-DASH media segments:
//!
//! - [`parse_sidx`] — parses a `SegmentIndexBox` (`sidx`) to obtain the
//!   segment's start and end presentation times in seconds.
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
//! # Limitations
//!
//! - Only top-level `sidx` boxes are scanned.  Hierarchical (nested) sidx
//!   chains are not followed.
//! - `sidx` boxes embedded inside `moof` are not found.
//! - Segments that use only `tfdt`/`mvhd` for timing (no `sidx`) will still
//!   produce `Chunk { start: None, end: None }` — this is the pre-existing
//!   behaviour and is not a regression.

/// Timing information extracted from an ISO BMFF `sidx` box.
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

// ── Internal helpers ──────────────────────────────────────────────────────────

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
}
