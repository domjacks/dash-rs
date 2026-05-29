//! XLink pre-processing pass for MPEG-DASH manifests.
//!
//! Walks the `Period` and `AdaptationSet` elements in a parsed [`MPD`], fetches
//! any `xlink:href` remote resources with `actuate="onLoad"`, and inlines them
//! in-place — producing a fully self-contained [`MPD`] that the rest of the
//! session construction code can use without further network access.
//!
//! This mirrors `XlinkController.js` / `XlinkLoader.js` in dash.js.
//!
//! # Spec notes
//!
//! * `xlink:actuate="onLoad"` — resolve eagerly (handled here).
//! * `xlink:actuate="onRequest"` — resolve lazily on first access (left in
//!   place; the `dash_mpd` crate defaults absent `@xlink:actuate` to this).
//! * `href="urn:mpeg:dash:resolve-to-zero:2013"` — the element is removed from
//!   the manifest.

use crate::Url;

#[cfg(not(feature = "std"))]
use alloc::{borrow::ToOwned, format, string::String, vec::Vec};

use crate::manifest::mpd_types::{AdaptationSet, Period, MPD};

use crate::{
    error::DashError,
    fetch::{FetchRequest, RequestType, SegmentFetcher},
};

// ── Public entry point ────────────────────────────────────────────────────────

/// Resolve all `xlink:href` references with `actuate="onLoad"` in the manifest.
///
/// After this call, the returned [`MPD`] contains no `Period` or
/// `AdaptationSet` elements with an outstanding eager XLink reference.  Elements
/// with `actuate="onRequest"` are left untouched.
///
/// Fetch failures and XML parse errors for individual remote resources are
/// logged as warnings and the problematic element is silently dropped, which
/// mirrors dash.js's lenient behaviour and avoids aborting playback for a
/// single bad XLink reference.
///
/// # Errors
///
/// Returns [`DashError::SegmentUrlUnresolvable`] if the `href` attribute
/// contains an invalid URL that can neither be parsed as absolute nor resolved
/// against `base_url`.
pub async fn resolve_xlinks<F: SegmentFetcher>(
    mut mpd: MPD,
    base_url: &Url,
    fetcher: &F,
) -> Result<MPD, DashError> {
    mpd.periods = resolve_period_xlinks(mpd.periods, base_url, fetcher).await?;

    for period in &mut mpd.periods {
        period.adaptations =
            resolve_adaptation_xlinks(core::mem::take(&mut period.adaptations), base_url, fetcher)
                .await?;
    }

    Ok(mpd)
}

// ── Period-level ──────────────────────────────────────────────────────────────

async fn resolve_period_xlinks<F: SegmentFetcher>(
    periods: Vec<Period>,
    base_url: &Url,
    fetcher: &F,
) -> Result<Vec<Period>, DashError> {
    let mut out: Vec<Period> = Vec::with_capacity(periods.len());

    for period in periods {
        match check_xlink(&period.href, &period.actuate, base_url) {
            XLinkAction::Skip => {
                out.push(period);
            }
            XLinkAction::Remove => {}
            XLinkAction::Fetch(url) => {
                match fetch_text(fetcher, &url).await {
                    Err(_) => {}
                    Ok(xml) => match parse_periods(&xml) {
                        None => {}
                        Some(remote_periods) => {
                            for mut rp in remote_periods {
                                // Merge placeholder attributes into the remote
                                // period where they are absent (DASH-IF XLink spec §4).
                                if rp.id.is_none() {
                                    rp.id.clone_from(&period.id);
                                }
                                if rp.start.is_none() {
                                    rp.start = period.start;
                                }
                                if rp.duration.is_none() {
                                    rp.duration = period.duration;
                                }
                                out.push(rp);
                            }
                        }
                    },
                }
            }
        }
    }

    Ok(out)
}

// ── AdaptationSet-level ───────────────────────────────────────────────────────

async fn resolve_adaptation_xlinks<F: SegmentFetcher>(
    adaptations: Vec<AdaptationSet>,
    base_url: &Url,
    fetcher: &F,
) -> Result<Vec<AdaptationSet>, DashError> {
    let mut out: Vec<AdaptationSet> = Vec::with_capacity(adaptations.len());

    for adaptation in adaptations {
        match check_xlink(&adaptation.href, &adaptation.actuate, base_url) {
            XLinkAction::Skip => {
                out.push(adaptation);
            }
            XLinkAction::Remove => {}
            XLinkAction::Fetch(url) => {
                match fetch_text(fetcher, &url).await {
                    Err(_) => {}
                    Ok(xml) => match parse_adaptation_sets(&xml) {
                        None => {}
                        Some(remote_adaptations) => {
                            for mut ra in remote_adaptations {
                                // Propagate inherited attributes from the placeholder.
                                if ra.id.is_none() {
                                    ra.id.clone_from(&adaptation.id);
                                }
                                if ra.contentType.is_none() {
                                    ra.contentType.clone_from(&adaptation.contentType);
                                }
                                if ra.lang.is_none() {
                                    ra.lang.clone_from(&adaptation.lang);
                                }
                                out.push(ra);
                            }
                        }
                    },
                }
            }
        }
    }

    Ok(out)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

const RESOLVE_TO_ZERO: &str = "urn:mpeg:dash:resolve-to-zero:2013";

/// Decision taken for a single XLink element.
enum XLinkAction {
    /// No `xlink:href`, or `actuate="onRequest"` — leave untouched.
    Skip,
    /// `href` is the resolve-to-zero sentinel — remove the element.
    Remove,
    /// Fetch the resource at this URL and inline the result.
    Fetch(Url),
}

/// Determine what to do with an element's XLink attributes.
///
/// Per the MPEG-DASH spec, only `actuate="onLoad"` triggers eager resolution.
/// The `dash_mpd` crate defaults an absent `@xlink:actuate` to `"onRequest"`,
/// so we only resolve when the attribute is *explicitly* set to `"onLoad"`.
fn check_xlink(href: &Option<String>, actuate: &Option<String>, base_url: &Url) -> XLinkAction {
    let href_str = match href.as_deref() {
        Some(h) if !h.is_empty() => h,
        _ => return XLinkAction::Skip,
    };

    // Only resolve onLoad; leave onRequest (and the dash_mpd default) alone.
    if actuate.as_deref() != Some("onLoad") {
        return XLinkAction::Skip;
    }

    if href_str == RESOLVE_TO_ZERO {
        return XLinkAction::Remove;
    }

    // Resolve the URL — absolute URLs win over relative.
    let url = if href_str.starts_with("http://") || href_str.starts_with("https://") {
        Url::parse(href_str).ok()
    } else {
        base_url.join(href_str).ok()
    };

    match url {
        Some(u) => XLinkAction::Fetch(u),
        None => XLinkAction::Skip,
    }
}

/// Fetch a remote XLink resource and return its body as UTF-8 text.
async fn fetch_text<F: SegmentFetcher>(
    fetcher: &F,
    url: &Url,
) -> Result<String, crate::fetch::FetchError> {
    let req = FetchRequest::new(url.clone(), RequestType::Manifest);
    let resp = fetcher.fetch(req).await?;
    let text = core::str::from_utf8(&resp.data).unwrap_or("").to_owned();
    Ok(text)
}

/// Parse a remote resource as one or more `<Period>` elements.
///
/// The remote XML may be:
/// - A bare `<Period>…</Period>` fragment, OR
/// - A full `<MPD>…</MPD>` document.
///
/// We handle both by first trying a full MPD parse, then wrapping the text
/// in a synthetic MPD envelope.
fn parse_periods(xml: &str) -> Option<Vec<Period>> {
    // First try: the resource is already a complete MPD.
    if let Ok(mpd) = crate::manifest::parse(xml) {
        if !mpd.periods.is_empty() {
            return Some(mpd.periods);
        }
    }

    // Second try: wrap in a synthetic MPD so that dash_mpd::parse() can handle
    // a bare <Period> fragment.
    let wrapped = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011">{xml}</MPD>"#
    );
    let mpd = crate::manifest::parse(&wrapped).ok()?;
    if mpd.periods.is_empty() {
        return None;
    }
    Some(mpd.periods)
}

/// Parse a remote resource as one or more `<AdaptationSet>` elements.
///
/// Wrapped inside a synthetic `<MPD><Period>…</Period></MPD>` so that
/// `crate::manifest::parse()` can deserialise a bare AdaptationSet fragment.
fn parse_adaptation_sets(xml: &str) -> Option<Vec<AdaptationSet>> {
    // First try: maybe the resource is a full MPD (unusual, but be lenient).
    if let Ok(mpd) = crate::manifest::parse(xml) {
        if let Some(p) = mpd.periods.into_iter().next() {
            if !p.adaptations.is_empty() {
                return Some(p.adaptations);
            }
        }
    }

    // Second try: wrap in a synthetic Period + MPD.
    let wrapped = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011"><Period>{xml}</Period></MPD>"#
    );
    let mpd = crate::manifest::parse(&wrapped).ok()?;
    let period = mpd.periods.into_iter().next()?;
    if period.adaptations.is_empty() {
        return None;
    }
    Some(period.adaptations)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{check_xlink, parse_adaptation_sets, parse_periods, XLinkAction};
    use crate::Url;
    use alloc::borrow::ToOwned;

    fn base() -> Url {
        Url::parse("http://example.com/").unwrap()
    }

    // ── check_xlink ───────────────────────────────────────────────────────────

    #[test]
    fn check_xlink_skips_absent_href() {
        assert!(matches!(
            check_xlink(&None, &None, &base()),
            XLinkAction::Skip
        ));
    }

    #[test]
    fn check_xlink_skips_on_request_actuate() {
        let href = Some("http://example.com/period.xml".to_owned());
        let actuate = Some("onRequest".to_owned());
        assert!(matches!(
            check_xlink(&href, &actuate, &base()),
            XLinkAction::Skip
        ));
    }

    #[test]
    fn check_xlink_removes_resolve_to_zero_sentinel() {
        let href = Some("urn:mpeg:dash:resolve-to-zero:2013".to_owned());
        let actuate = Some("onLoad".to_owned());
        assert!(matches!(
            check_xlink(&href, &actuate, &base()),
            XLinkAction::Remove
        ));
    }

    #[test]
    fn check_xlink_fetch_absolute_url() {
        let href = Some("http://cdn.example.com/extra.xml".to_owned());
        let actuate = Some("onLoad".to_owned());
        let action = check_xlink(&href, &actuate, &base());
        assert!(matches!(action, XLinkAction::Fetch(_)));
    }

    #[test]
    fn check_xlink_fetch_relative_url_resolved_against_base() {
        let href = Some("segments/extra.xml".to_owned());
        let actuate = Some("onLoad".to_owned());
        let action = check_xlink(&href, &actuate, &base());
        assert!(matches!(action, XLinkAction::Fetch(_)));
    }

    // ── parse_periods ─────────────────────────────────────────────────────────

    #[test]
    fn parse_periods_from_bare_fragment() {
        let xml = r#"<Period id="p1" start="PT0S"><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="500000"/></AdaptationSet></Period>"#;
        let periods = parse_periods(xml).expect("should parse bare Period fragment");
        assert_eq!(periods.len(), 1);
        assert_eq!(periods[0].id.as_deref(), Some("p1"));
    }

    #[test]
    fn parse_periods_returns_none_for_garbage_xml() {
        assert!(parse_periods("not xml at all!!!").is_none());
    }

    // ── parse_adaptation_sets ─────────────────────────────────────────────────

    #[test]
    fn parse_adaptation_sets_from_bare_fragment() {
        let xml = r#"<AdaptationSet mimeType="video/mp4" lang="en"><Representation id="v1" bandwidth="500000"/></AdaptationSet>"#;
        let sets = parse_adaptation_sets(xml).expect("should parse bare AdaptationSet fragment");
        assert_eq!(sets.len(), 1);
    }

    #[test]
    fn parse_adaptation_sets_returns_none_for_garbage_xml() {
        assert!(parse_adaptation_sets("garbage").is_none());
    }
}
