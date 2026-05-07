//! URL template expansion and BaseURL resolution (`segment_url`).
//!
//! Handles the three URL-building responsibilities described in DASH spec §5.3.9:
//!
//! - **`expand_template`** — substitutes `$RepresentationID$`, `$Bandwidth$`,
//!   `$Number$`, and `$Time$` tokens in a `SegmentTemplate@media` or
//!   `@initialization` string, including `%0Nd` zero-padding.
//! - **`collect_candidate_base_urls`** — builds the ordered list of CDN
//!   candidates by chaining `BaseURL` elements at every hierarchy level
//!   (manifest URL → MPD → Period → AdaptationSet → Representation).
//! - **`resolve_effective_base_url`** — picks the single highest-priority
//!   base URL for init-segment resolution (deterministic, no random sampling).
//!
//! Mirrors `TemplateSegmentsGetter.js` and parts of `URLLoader.js` in dash.js.

use dash_mpd::{AdaptationSet, BaseURL, Period};
use url::Url;

/// Expand DASH `SegmentTemplate` identifier tokens.
///
/// Handled identifiers:
/// - `$RepresentationID$`
/// - `$Bandwidth$`
/// - `$Number$` and `$Number%0{width}d$` (zero-padded decimal)
/// - `$Time$` and `$Time%0{width}d$` (zero-padded decimal)
///
/// `time` must be `Some` when expanding a `$Time$`-addressed template (i.e.
/// one with a `SegmentTimeline`); pass `None` for `$Number$`-addressed
/// templates.  An unmatched `$Time$` with `time = None` is reproduced
/// verbatim so as not to silently corrupt a URL.
///
/// Unknown identifiers are reproduced verbatim (including the surrounding
/// `$` delimiters) so as not to silently corrupt a URL.
pub(super) fn expand_template(
    tmpl: &str,
    rep_id: &str,
    bandwidth: u64,
    number: u64,
    time: Option<u64>,
) -> String {
    let mut out = String::with_capacity(tmpl.len() + 32);
    let mut s = tmpl;

    while let Some(open) = s.find('$') {
        out.push_str(&s[..open]);
        s = &s[open + 1..]; // skip opening '$'

        if let Some(close) = s.find('$') {
            let token = &s[..close]; // e.g. "Number%06d", "RepresentationID"
            s = &s[close + 1..]; // skip closing '$'

            // Split token into identifier and optional format specifier.
            let (name, fmt) = if let Some(pct) = token.find('%') {
                (&token[..pct], Some(&token[pct..]))
            } else {
                (token, None)
            };

            /// Format a u64 value with an optional `%0{width}d` specifier.
            fn fmt_value(value: u64, fmt: Option<&str>, out: &mut String) {
                let mut formatted = false;
                if let Some(f) = fmt {
                    if f.starts_with("%0") && f.ends_with('d') {
                        if let Ok(w) = f[2..f.len() - 1].parse::<usize>() {
                            out.push_str(&format!("{:0>width$}", value, width = w));
                            formatted = true;
                        }
                    }
                }
                if !formatted {
                    out.push_str(&value.to_string());
                }
            }

            match name {
                "RepresentationID" => out.push_str(rep_id),
                "Bandwidth" => out.push_str(&bandwidth.to_string()),
                "Number" => fmt_value(number, fmt, &mut out),
                "Time" => {
                    if let Some(t) = time {
                        fmt_value(t, fmt, &mut out);
                    } else {
                        // No time value — reproduce verbatim.
                        out.push('$');
                        out.push_str(token);
                        out.push('$');
                    }
                }
                _ => {
                    // Unknown identifier — reproduce verbatim.
                    out.push('$');
                    out.push_str(token);
                    out.push('$');
                }
            }
        } else {
            // Unterminated '$' — pass through literally.
            out.push('$');
        }
    }

    out.push_str(s);
    out
}

/// Collect candidate base URLs for a segment fetch by chaining `BaseURL`
/// elements at every level of the DASH hierarchy (DASH spec §5.10.1.3):
///
///   manifest URL → MPD → Period → AdaptationSet → Representation
///
/// At each level that declares `BaseURL` elements, each parent candidate is
/// combined with each child `BaseURL` (in priority / weight order) to produce
/// the next level's candidates.  Levels with no `BaseURL` elements are passed
/// through unchanged.
///
/// The list is guaranteed to be non-empty (falls back to the manifest URL).
pub(super) fn collect_candidate_base_urls(
    manifest_url: &Url,
    mpd_base_urls: &[BaseURL],
    period: &Period,
    adaptation: &AdaptationSet,
    repr_base_urls: &[BaseURL],
) -> Vec<Url> {
    fn expand(parents: Vec<Url>, bases: &[BaseURL]) -> Vec<Url> {
        if bases.is_empty() {
            return parents;
        }
        let ordered = priority_weighted_order(bases);
        parents
            .iter()
            .flat_map(|p| ordered.iter().filter_map(move |b| p.join(&b.base).ok()))
            .collect()
    }

    let root = vec![manifest_url.clone()];
    let after_mpd = expand(root, mpd_base_urls);
    let after_period = expand(after_mpd, &period.BaseURL);
    let after_adapt = expand(after_period, &adaptation.BaseURL);
    expand(after_adapt, repr_base_urls)
}

/// Resolve a single effective base URL for init-segment URL construction by
/// deterministically walking the `BaseURL` chain from the manifest URL through
/// every hierarchy level (MPD → Period → AdaptationSet → Representation).
///
/// Unlike [`collect_candidate_base_urls`], which produces a full candidate
/// list for CDN failover, this picks the **first** (highest-priority) element
/// at each level and chains them.  Suitable for one-shot URL resolution where
/// a single base is required.
pub(super) fn resolve_effective_base_url(
    manifest_url: &Url,
    mpd_base_urls: &[BaseURL],
    period: &Period,
    adaptation: &AdaptationSet,
    repr_base_urls: &[BaseURL],
) -> Url {
    let mut base = manifest_url.clone();
    for level in [
        mpd_base_urls,
        &period.BaseURL,
        &adaptation.BaseURL,
        repr_base_urls,
    ] {
        if let Some(b) = level.first() {
            if let Ok(u) = base.join(&b.base) {
                base = u;
            }
        }
    }
    base
}

/// Sort a slice of `BaseURL` elements by priority ascending with
/// weight-proportional random ordering within each same-priority tier.
///
/// Uses the Efraimidis–Spirakis algorithm: each item receives a score
/// `U^(1/weight)` where U is uniform random; higher scores come first.
/// This produces a correctly weighted random permutation within each tier.
fn priority_weighted_order(bases: &[BaseURL]) -> Vec<&BaseURL> {
    use rand::Rng;
    let mut rng = rand::thread_rng();

    let mut scored: Vec<(u64, f64, &BaseURL)> = bases
        .iter()
        .map(|b| {
            let priority = b.priority.unwrap_or(1);
            let weight = b.weight.unwrap_or(1).max(1) as f64;
            let u: f64 = rng.gen::<f64>().max(1e-10_f64);
            let score = u.powf(1.0 / weight);
            (priority, score, b)
        })
        .collect();

    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    scored.into_iter().map(|(_, _, b)| b).collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::expand_template;

    #[test]
    fn expand_plain_number() {
        assert_eq!(
            expand_template("seg_$Number$.m4s", "r1", 1000, 42, None),
            "seg_42.m4s"
        );
    }

    #[test]
    fn expand_zero_padded_number_width6() {
        assert_eq!(
            expand_template(
                "$RepresentationID$/$Number%06d$.m4s",
                "1920x1080p25",
                4741120,
                1,
                None,
            ),
            "1920x1080p25/000001.m4s"
        );
    }

    #[test]
    fn expand_zero_padded_number_width3() {
        assert_eq!(
            expand_template("$Number%03d$.m4s", "id", 999, 7, None),
            "007.m4s"
        );
    }

    #[test]
    fn expand_representation_id_and_bandwidth() {
        assert_eq!(
            expand_template("$RepresentationID$_$Bandwidth$.mp4", "hd", 5000000, 0, None),
            "hd_5000000.mp4"
        );
    }

    /// $Time$ with time=None → reproduced verbatim (no time value supplied).
    #[test]
    fn expand_time_none_reproduced_verbatim() {
        assert_eq!(
            expand_template("$Time$.m4s", "id", 100, 1, None),
            "$Time$.m4s"
        );
    }

    /// $Time$ with a concrete value should be substituted.
    #[test]
    fn expand_time_basic() {
        assert_eq!(
            expand_template("chunk_$Time$.m4s", "r1", 1000, 0, Some(90_000)),
            "chunk_90000.m4s"
        );
    }

    /// $Time$ with zero-padding format specifier.
    #[test]
    fn expand_time_zero_padded() {
        assert_eq!(
            expand_template("$Time%09d$.m4s", "r1", 1000, 0, Some(90_000)),
            "000090000.m4s"
        );
    }

    #[test]
    fn expand_unterminated_dollar_preserved() {
        assert_eq!(expand_template("seg$", "id", 100, 1, None), "seg$");
    }
}
