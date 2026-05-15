//! Minimal `no_std` URL newtype used when the `std` feature is disabled.
//!
//! Replaces `url::Url` in no_std environments.  No RFC 3986 validation is
//! performed — the string is stored as-is.  Only the methods actually called
//! by the rest of the library are implemented.

use alloc::borrow::ToOwned;
use alloc::string::{String, ToString};
use core::fmt;

/// A minimal URL type for `no_std` environments.
///
/// Wraps a URL string without RFC 3986 validation.  Only used when the `std`
/// feature is disabled; when `std` is enabled the real `url::Url` crate is
/// used instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url(String);

/// Error returned by [`Url::parse`] or [`Url::join`].
///
/// Currently never returned (no validation is performed), but kept for API
/// compatibility with `url::Url`.
#[derive(Debug)]
pub struct UrlError;

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid URL")
    }
}

impl Url {
    /// Parse a URL from a string slice.  Always succeeds (no validation).
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        Ok(Self(s.to_owned()))
    }

    /// Resolve a relative reference against this URL.
    ///
    /// - Absolute `other` strings (`http://` / `https://`) are returned as-is.
    /// - Relative strings are appended to the base path (last path segment
    ///   stripped if the base does not end with `/`).
    pub fn join(&self, other: &str) -> Result<Self, UrlError> {
        if other.starts_with("http://") || other.starts_with("https://") {
            return Ok(Self(other.to_owned()));
        }
        let base = self.0.as_str();
        let joined = if base.ends_with('/') {
            alloc::format!("{}{}", base, other)
        } else {
            let up_to_last_slash = base.rfind('/').map(|i| i + 1).unwrap_or(base.len());
            alloc::format!("{}{}", &base[..up_to_last_slash], other)
        };
        Ok(Self(joined))
    }

    /// Returns the URL as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the host component, or `None` if it cannot be parsed.
    pub fn host_str(&self) -> Option<&str> {
        let s = self.0.as_str();
        let after_scheme = s.find("://").map(|i| &s[i + 3..])?;
        // Strip any `user:pass@` prefix.
        let host_start = after_scheme
            .find('@')
            .map(|i| &after_scheme[i + 1..])
            .unwrap_or(after_scheme);
        let host_end = host_start
            .find(|c| c == '/' || c == ':' || c == '?')
            .unwrap_or(host_start.len());
        if host_end == 0 {
            None
        } else {
            Some(&host_start[..host_end])
        }
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
