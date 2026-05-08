//! MPD XML parser.
//!
//! Wraps `quick-xml`'s serde deserializer and exposes a single
//! [`parse`] entry point that mirrors the `dash_mpd::parse` API.

use crate::manifest::mpd_types::MPD;

// ── Error type ────────────────────────────────────────────────────────────────

/// Error returned when an MPD manifest cannot be parsed.
#[derive(Debug, thiserror::Error)]
#[error("MPD parse error: {0}")]
pub struct MpdParseError(String);

// ── Public entry point ────────────────────────────────────────────────────────

/// Parse an MPEG-DASH manifest from raw XML text.
///
/// Mirrors `dash_mpd::parse` — same function signature, different backing
/// implementation (vendored `quick-xml` + `serde` instead of the `dash-mpd`
/// crate).
///
/// Unknown elements and attributes are silently ignored.
///
/// # Errors
///
/// Returns [`MpdParseError`] if the XML is malformed or contains values that
/// fail validation (e.g. an unparseable ISO 8601 duration).
pub fn parse(xml: &str) -> Result<MPD, MpdParseError> {
    quick_xml::de::from_str(xml).map_err(|e| MpdParseError(e.to_string()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::parse;

    /// Verify that quick-xml 0.36 strips namespace prefixes from attribute names
    /// (`xlink:href` → `@href`, `xlink:actuate` → `@actuate`), which is why
    /// `mpd_types.rs` uses `#[serde(rename = "@href")]` instead of
    /// `#[serde(rename = "@xlink:href")]`.
    #[test]
    fn xlink_attrs_are_parsed() {
        let xml = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     xmlns:xlink="http://www.w3.org/1999/xlink"
     type="static">
  <Period xlink:href="http://example.com/p.mpd"
          xlink:actuate="onLoad"/>
</MPD>"#;
        let mpd = parse(xml).expect("parse failed");
        assert_eq!(mpd.periods.len(), 1);
        let p = &mpd.periods[0];
        assert_eq!(p.href.as_deref(), Some("http://example.com/p.mpd"), "href missing");
        assert_eq!(p.actuate.as_deref(), Some("onLoad"), "actuate missing");
    }

    /// Smoke test: a realistic multi-representation MPD round-trips correctly.
    #[test]
    fn static_mpd_basic_fields() {
        let xml = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     type="static"
     mediaPresentationDuration="PT30S">
  <Period duration="PT30S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4"
                       timescale="90000" duration="360000" startNumber="1"/>
      <Representation id="r1" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let mpd = parse(xml).expect("parse failed");
        assert_eq!(mpd.mpdtype.as_deref(), Some("static"));
        assert!(mpd.mediaPresentationDuration.is_some());
        assert_eq!(mpd.periods.len(), 1);
        let adaptation = &mpd.periods[0].adaptations[0];
        assert_eq!(adaptation.mimeType.as_deref(), Some("video/mp4"));
        let repr = &adaptation.representations[0];
        assert_eq!(repr.bandwidth, Some(1_000_000));
        assert_eq!(repr.width, Some(1280));
    }
}
