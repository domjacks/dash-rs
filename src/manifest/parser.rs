//! MPD XML parser.
//!
//! Uses `roxmltree` to walk the element tree and populate the [`MPD`] struct.

use crate::manifest::mpd_types::{
    AdaptationSet, BaseURL, Initialization, Latency, Location, Period, PlaybackRate,
    Representation, SegmentBase, SegmentList, SegmentTemplate, SegmentTimeline, SegmentURL,
    ServiceDescription, UTCTiming, MPD, S,
};
use crate::util::{parse_iso_duration, parse_xs_datetime};

#[cfg(not(feature = "std"))]
use alloc::{
    borrow::ToOwned,
    string::{String, ToString},
};

// ── xlink namespace ───────────────────────────────────────────────────────────

const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

// ── Error type ────────────────────────────────────────────────────────────────

/// Error returned when an MPD manifest cannot be parsed.
#[derive(Debug, thiserror::Error, PartialEq)]
#[error("MPD parse error: {0}")]
pub struct MpdParseError(String);

// ── Public entry point ────────────────────────────────────────────────────────

/// Parse an MPEG-DASH manifest from raw XML text.
///
/// Mirrors `dash_mpd::parse` — same function signature, different backing
/// implementation (`roxmltree` tree walk instead of `quick-xml` + `serde`).
///
/// Unknown elements and attributes are silently ignored.
///
/// # Errors
///
/// Returns [`MpdParseError`] if the XML is malformed.
pub fn parse(xml: &str) -> Result<MPD, MpdParseError> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| MpdParseError(e.to_string()))?;
    let root = doc.root_element();
    Ok(parse_mpd(root))
}

// ── Attribute helpers ─────────────────────────────────────────────────────────

/// Read a plain string attribute, returning `None` when absent.
fn attr_str<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attribute(name)
}

/// Read an attribute and parse it via `FromStr`.
fn attr<T: core::str::FromStr>(node: roxmltree::Node<'_, '_>, name: &str) -> Option<T> {
    node.attribute(name)?.parse().ok()
}

/// Read an ISO 8601 duration attribute.
fn attr_duration(node: roxmltree::Node<'_, '_>, name: &str) -> Option<core::time::Duration> {
    parse_iso_duration(node.attribute(name)?)
}

/// Read an XS dateTime attribute as Unix seconds.
fn attr_xs_datetime(node: roxmltree::Node<'_, '_>, name: &str) -> Option<f64> {
    parse_xs_datetime(node.attribute(name)?)
}

/// Read an xlink:href attribute, falling back to plain `href`.
fn attr_xlink_href(node: roxmltree::Node<'_, '_>) -> Option<String> {
    node.attribute((XLINK_NS, "href"))
        .or_else(|| node.attribute("href"))
        .map(ToOwned::to_owned)
}

/// Read an xlink:actuate attribute, falling back to plain `actuate`.
fn attr_xlink_actuate(node: roxmltree::Node<'_, '_>) -> Option<String> {
    node.attribute((XLINK_NS, "actuate"))
        .or_else(|| node.attribute("actuate"))
        .map(ToOwned::to_owned)
}

// ── Element parsers ───────────────────────────────────────────────────────────

fn parse_mpd(node: roxmltree::Node<'_, '_>) -> MPD {
    let mut mpd = MPD {
        mpdtype: attr_str(node, "type").map(ToOwned::to_owned),
        profiles: attr_str(node, "profiles").map(ToOwned::to_owned),
        mediaPresentationDuration: attr_duration(node, "mediaPresentationDuration"),
        minimumUpdatePeriod: attr_duration(node, "minimumUpdatePeriod"),
        minBufferTime: attr_duration(node, "minBufferTime"),
        availabilityStartTime: attr_xs_datetime(node, "availabilityStartTime"),
        availabilityEndTime: attr_xs_datetime(node, "availabilityEndTime"),
        suggestedPresentationDelay: attr_duration(node, "suggestedPresentationDelay"),
        timeShiftBufferDepth: attr_duration(node, "timeShiftBufferDepth"),
        ..MPD::default()
    };

    for child in node.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "Period" => mpd.periods.push(parse_period(child)),
            "BaseURL" => mpd.base_url.push(parse_base_url(child)),
            "Location" => mpd.locations.push(parse_location(child)),
            "UTCTiming" => mpd.UTCTiming.push(parse_utc_timing(child)),
            "ServiceDescription" => mpd
                .ServiceDescription
                .push(parse_service_description(child)),
            _ => {}
        }
    }

    mpd
}

fn parse_period(node: roxmltree::Node<'_, '_>) -> Period {
    let mut p = Period {
        id: attr_str(node, "id").map(ToOwned::to_owned),
        start: attr_duration(node, "start"),
        duration: attr_duration(node, "duration"),
        href: attr_xlink_href(node),
        actuate: attr_xlink_actuate(node),
        ..Period::default()
    };

    for child in node.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "AdaptationSet" => p.adaptations.push(parse_adaptation_set(child)),
            "BaseURL" => p.BaseURL.push(parse_base_url(child)),
            "SegmentTemplate" => p.SegmentTemplate = Some(parse_segment_template(child)),
            "SegmentList" => p.SegmentList = Some(parse_segment_list(child)),
            "SegmentBase" => p.SegmentBase = Some(parse_segment_base(child)),
            _ => {}
        }
    }

    p
}

fn parse_adaptation_set(node: roxmltree::Node<'_, '_>) -> AdaptationSet {
    let mut a = AdaptationSet {
        id: attr_str(node, "id").map(ToOwned::to_owned),
        contentType: attr_str(node, "contentType").map(ToOwned::to_owned),
        mimeType: attr_str(node, "mimeType").map(ToOwned::to_owned),
        lang: attr_str(node, "lang").map(ToOwned::to_owned),
        codecs: attr_str(node, "codecs").map(ToOwned::to_owned),
        href: attr_xlink_href(node),
        actuate: attr_xlink_actuate(node),
        ..AdaptationSet::default()
    };

    for child in node.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "Representation" => a.representations.push(parse_representation(child)),
            "BaseURL" => a.BaseURL.push(parse_base_url(child)),
            "SegmentTemplate" => a.SegmentTemplate = Some(parse_segment_template(child)),
            "SegmentList" => a.SegmentList = Some(parse_segment_list(child)),
            "SegmentBase" => a.SegmentBase = Some(parse_segment_base(child)),
            _ => {}
        }
    }

    a
}

fn parse_representation(node: roxmltree::Node<'_, '_>) -> Representation {
    let mut r = Representation {
        id: attr_str(node, "id").map(ToOwned::to_owned),
        bandwidth: attr(node, "bandwidth"),
        mimeType: attr_str(node, "mimeType").map(ToOwned::to_owned),
        codecs: attr_str(node, "codecs").map(ToOwned::to_owned),
        width: attr(node, "width"),
        height: attr(node, "height"),
        ..Representation::default()
    };

    for child in node.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "BaseURL" => r.BaseURL.push(parse_base_url(child)),
            "SegmentTemplate" => r.SegmentTemplate = Some(parse_segment_template(child)),
            "SegmentList" => r.SegmentList = Some(parse_segment_list(child)),
            "SegmentBase" => r.SegmentBase = Some(parse_segment_base(child)),
            _ => {}
        }
    }

    r
}

fn parse_segment_template(node: roxmltree::Node<'_, '_>) -> SegmentTemplate {
    let mut st = SegmentTemplate {
        media: attr_str(node, "media").map(ToOwned::to_owned),
        initialization: attr_str(node, "initialization").map(ToOwned::to_owned),
        startNumber: attr(node, "startNumber"),
        duration: attr(node, "duration"),
        timescale: attr(node, "timescale"),
        presentationTimeOffset: attr(node, "presentationTimeOffset"),
        availabilityTimeOffset: attr(node, "availabilityTimeOffset"),
        availabilityTimeComplete: node
            .attribute("availabilityTimeComplete")
            .map(|v| v.eq_ignore_ascii_case("true")),
        ..SegmentTemplate::default()
    };

    for child in node.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "Initialization" => st.Initialization = Some(parse_initialization(child)),
            "SegmentTimeline" => st.SegmentTimeline = Some(parse_segment_timeline(child)),
            _ => {}
        }
    }

    st
}

fn parse_segment_list(node: roxmltree::Node<'_, '_>) -> SegmentList {
    let mut sl = SegmentList {
        duration: attr(node, "duration"),
        timescale: attr(node, "timescale"),
        startNumber: attr(node, "startNumber"),
        presentationTimeOffset: attr(node, "presentationTimeOffset"),
        ..SegmentList::default()
    };

    for child in node.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "SegmentURL" => sl.segment_urls.push(parse_segment_url(child)),
            "Initialization" => sl.Initialization = Some(parse_initialization(child)),
            "SegmentTimeline" => sl.SegmentTimeline = Some(parse_segment_timeline(child)),
            _ => {}
        }
    }

    sl
}

fn parse_segment_url(node: roxmltree::Node<'_, '_>) -> SegmentURL {
    SegmentURL {
        media: attr_str(node, "media").map(ToOwned::to_owned),
        mediaRange: attr_str(node, "mediaRange").map(ToOwned::to_owned),
    }
}

fn parse_segment_base(node: roxmltree::Node<'_, '_>) -> SegmentBase {
    let mut sb = SegmentBase {
        timescale: attr(node, "timescale"),
        presentationTimeOffset: attr(node, "presentationTimeOffset"),
        indexRange: attr_str(node, "indexRange").map(ToOwned::to_owned),
        ..SegmentBase::default()
    };

    for child in node.children().filter(|n| n.is_element()) {
        if child.tag_name().name() == "Initialization" {
            sb.Initialization = Some(parse_initialization(child));
        }
    }

    sb
}

fn parse_segment_timeline(node: roxmltree::Node<'_, '_>) -> SegmentTimeline {
    let mut tl = SegmentTimeline::default();

    for child in node.children().filter(|n| n.is_element()) {
        if child.tag_name().name() == "S" {
            tl.segments.push(parse_s(child));
        }
    }

    tl
}

fn parse_s(node: roxmltree::Node<'_, '_>) -> S {
    S {
        t: attr(node, "t"),
        d: attr(node, "d").unwrap_or(0),
        r: attr(node, "r"),
    }
}

fn parse_initialization(node: roxmltree::Node<'_, '_>) -> Initialization {
    Initialization {
        sourceURL: attr_str(node, "sourceURL").map(ToOwned::to_owned),
        range: attr_str(node, "range").map(ToOwned::to_owned),
    }
}

fn parse_base_url(node: roxmltree::Node<'_, '_>) -> BaseURL {
    BaseURL {
        base: node.text().unwrap_or("").to_owned(),
        serviceLocation: attr_str(node, "serviceLocation").map(ToOwned::to_owned),
        priority: attr(node, "priority"),
        weight: attr(node, "weight"),
    }
}

fn parse_location(node: roxmltree::Node<'_, '_>) -> Location {
    Location {
        url: node.text().unwrap_or("").to_owned(),
    }
}

fn parse_utc_timing(node: roxmltree::Node<'_, '_>) -> UTCTiming {
    UTCTiming {
        schemeIdUri: attr_str(node, "schemeIdUri").unwrap_or("").to_owned(),
        value: attr_str(node, "value").map(ToOwned::to_owned),
    }
}

fn parse_service_description(node: roxmltree::Node<'_, '_>) -> ServiceDescription {
    let mut sd = ServiceDescription::default();

    for child in node.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "Latency" => sd.Latency.push(Latency {
                target: attr(child, "target"),
                max: attr(child, "max"),
                min: attr(child, "min"),
            }),
            "PlaybackRate" => sd.PlaybackRate.push(PlaybackRate {
                max: attr(child, "max"),
                min: attr(child, "min"),
            }),
            _ => {}
        }
    }

    sd
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::parse;

    /// Verify that xlink:href / xlink:actuate are read via the xlink namespace.
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
        assert_eq!(
            p.href.as_deref(),
            Some("http://example.com/p.mpd"),
            "href missing"
        );
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
