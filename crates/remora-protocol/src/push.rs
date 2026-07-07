//! Push-wake registration and endpoint validation (ADR-0023).
//!
//! v1 delivers wakes over UnifiedPush: a device registers a distributor
//! endpoint URL with its own bridge (see [`crate::RemoteOp::RegisterPushEndpoint`]),
//! the bridge asserts it to the relay alongside routing credentials (see
//! [`crate::AssertedDevice::push`]), and the relay POSTs a fixed generic
//! wake body to that URL when the bridge's tee sees a hosted session block.
//! [`validate_push_endpoint`] is syntax-only: it bounds what a bridge or
//! client will accept as a candidate endpoint before storing/asserting it.
//! The relay's network-target (SSRF) safety policy — resolving and checking
//! the destination address, pinning it, disabling redirects — is a
//! delivery-time concern of the relay, not this crate; this crate is
//! dependency-light and does no DNS/network I/O.

use serde::{Deserialize, Serialize};

/// Maximum length of a push endpoint URL, in bytes.
pub const MAX_PUSH_ENDPOINT_LEN: usize = 2048;

/// A device's registered push-wake channel (ADR-0023).
///
/// `#[non_exhaustive]`: v1 ships one variant, `UnifiedPush`. An APNs/FCM
/// path or the future Remora-operated gateway (ADR-0023's deferred scope)
/// arrives as a new variant, not a breaking reshape of this type or the
/// roster/wire state that carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PushRegistration {
    /// A UnifiedPush distributor endpoint. The relay POSTs a fixed, generic
    /// wake body to this URL — never session identity or content — when the
    /// registering device's bridge sees a hosted session need attention.
    /// Callers should validate with [`validate_push_endpoint`] before
    /// storing or asserting a value of this variant.
    UnifiedPush {
        /// The distributor's subscribe/publish URL, e.g. an ntfy topic.
        endpoint: String,
    },
}

/// Error returned when [`validate_push_endpoint`] rejects a candidate
/// endpoint URL.
///
/// `#[non_exhaustive]`: this is a syntax-only check today; future variants
/// may distinguish more failure shapes without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushEndpointError {
    /// Not a syntactically valid `scheme://authority...` URL (includes
    /// empty input, and whitespace/control characters anywhere in it).
    InvalidUrl,
    /// The scheme was not `http` or `https`.
    UnsupportedScheme,
    /// The authority carried userinfo (`user@host`), which push endpoints
    /// must not.
    UserInfo,
    /// The URL carried a fragment (`#...`), which push endpoints must not.
    Fragment,
    /// The host component was empty.
    EmptyHost,
    /// The URL exceeded [`MAX_PUSH_ENDPOINT_LEN`] bytes.
    TooLong(usize),
}

impl std::fmt::Display for PushEndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushEndpointError::InvalidUrl => write!(f, "not a valid URL"),
            PushEndpointError::UnsupportedScheme => {
                write!(f, "unsupported URL scheme: expected http or https")
            }
            PushEndpointError::UserInfo => {
                write!(f, "URL must not contain userinfo (user@host)")
            }
            PushEndpointError::Fragment => {
                write!(f, "URL must not contain a fragment (#...)")
            }
            PushEndpointError::EmptyHost => write!(f, "URL host must not be empty"),
            PushEndpointError::TooLong(len) => {
                write!(
                    f,
                    "push endpoint URL too long: {len} bytes exceeds max {MAX_PUSH_ENDPOINT_LEN}"
                )
            }
        }
    }
}

impl std::error::Error for PushEndpointError {}

/// Validates `url` as a candidate push endpoint: syntax only.
///
/// Checks (in order): total length ≤ [`MAX_PUSH_ENDPOINT_LEN`] bytes; no
/// whitespace or control characters; a parseable `scheme://authority...`
/// shape; scheme is `http` or `https`; authority carries no userinfo
/// (`user@`); no fragment (`#...`); host component is non-empty — including
/// the *inside* of a bracketed IPv6 literal, so `https://[]:8080/x` is
/// rejected, as are an unterminated `[` and junk between `]` and the port
/// (#290). Does not resolve the host or otherwise touch the network — that
/// is the relay's delivery-time SSRF policy (ADR-0023), not this crate's
/// concern.
pub fn validate_push_endpoint(url: &str) -> Result<(), PushEndpointError> {
    if url.len() > MAX_PUSH_ENDPOINT_LEN {
        return Err(PushEndpointError::TooLong(url.len()));
    }
    if url.is_empty() || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(PushEndpointError::InvalidUrl);
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(PushEndpointError::InvalidUrl);
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(PushEndpointError::UnsupportedScheme);
    }
    if rest.contains('#') {
        return Err(PushEndpointError::Fragment);
    }
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return Err(PushEndpointError::UserInfo);
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        // Bracketed IPv6 literal: the host is what sits inside the brackets,
        // and it must not be empty — a naive split-on-':' saw `[` and let the
        // degenerate `https://[]:8080/x` through (#290).
        let Some(end) = bracketed.find(']') else {
            // Unterminated `[...` is not a valid authority at all.
            return Err(PushEndpointError::InvalidUrl);
        };
        // After `]` only nothing or a `:port` may follow.
        let after = &bracketed[end + 1..];
        if !(after.is_empty() || after.starts_with(':')) {
            return Err(PushEndpointError::InvalidUrl);
        }
        &bracketed[..end]
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return Err(PushEndpointError::EmptyHost);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_registration_round_trips() {
        let msg = PushRegistration::UnifiedPush {
            endpoint: "https://ntfy.sh/topic".to_string(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"unified_push":{"endpoint":"https://ntfy.sh/topic"}}"#
        );
        let back: PushRegistration = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, msg);
    }

    #[test]
    fn validate_push_endpoint_edge_table() {
        let ok = [
            "https://ntfy.sh/topic",
            "http://192.168.1.10:8080/t",
            "https://[::1]:8080/t",    // bracketed IPv6 literal with port
            "https://[fd00::1]/topic", // bracketed IPv6 literal, default port
        ];
        for u in ok {
            assert!(validate_push_endpoint(u).is_ok(), "{u}");
        }
        let bad = [
            "file:///etc/passwd",
            "gopher://x/",
            "ftp://x/",
            "https://user:pw@host/t", // userinfo
            "https://host/t#frag",    // fragment
            "https:///nohost",        // empty host
            "not a url",
            "",
        ];
        for u in bad {
            assert!(validate_push_endpoint(u).is_err(), "{u}");
        }
        let long = format!("https://h/{}", "a".repeat(2100));
        assert!(validate_push_endpoint(&long).is_err());
    }

    /// Degenerate host shapes (#290): an explicitly empty bracketed IPv6 host
    /// used to slip past the naive split-on-':' host check; it and its
    /// neighbours (unterminated bracket, junk after the bracket, port-only
    /// authority) are all rejected now.
    #[test]
    fn validate_push_endpoint_rejects_degenerate_hosts() {
        assert_eq!(
            validate_push_endpoint("https://[]:8080/x"),
            Err(PushEndpointError::EmptyHost),
            "empty bracketed IPv6 host with port"
        );
        assert_eq!(
            validate_push_endpoint("https://[]/x"),
            Err(PushEndpointError::EmptyHost),
            "empty bracketed IPv6 host without port"
        );
        assert_eq!(
            validate_push_endpoint("https://[]"),
            Err(PushEndpointError::EmptyHost),
            "empty bracketed IPv6 host, bare authority"
        );
        assert_eq!(
            validate_push_endpoint("https://[::1/x"),
            Err(PushEndpointError::InvalidUrl),
            "unterminated bracket"
        );
        assert_eq!(
            validate_push_endpoint("https://[::1]junk:8080/x"),
            Err(PushEndpointError::InvalidUrl),
            "junk between the closing bracket and the port"
        );
        assert_eq!(
            validate_push_endpoint("https://:8080/x"),
            Err(PushEndpointError::EmptyHost),
            "port-only authority"
        );
    }
}
