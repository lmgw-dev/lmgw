//! What every answer of the gateway's own origin sends (machine-check design
//! WP13, D25, D90): `X-Content-Type-Options: nosniff` and
//! `Content-Security-Policy: sandbox`, so a stored HTML or SVG file, or
//! anything relayed, opened in a window at the gateway's origin runs nothing.

use reqwest::header::HeaderMap;

/// The answer's `Content-Security-Policy` values, in order.
pub fn policies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("content-security-policy")
        .iter()
        .map(|v| v.to_str().unwrap_or("<not text>").to_string())
        .collect()
}

/// How many of the answer's policies have a bare `sandbox` directive.
pub fn bare_sandboxes(headers: &HeaderMap) -> usize {
    policies(headers)
        .iter()
        .filter(|p| {
            p.split([',', ';'])
                .any(|d| d.trim().eq_ignore_ascii_case("sandbox"))
        })
        .count()
}

/// `nosniff`, and `sandbox` as the answer's one policy — once, however many
/// layers set it; `what` names the answer in the failure.
pub fn assert_sandboxed(headers: &HeaderMap, what: &str) {
    let nosniff: Vec<_> = headers
        .get_all("x-content-type-options")
        .iter()
        .map(|v| v.to_str().unwrap_or_default())
        .collect();
    assert_eq!(nosniff, ["nosniff"], "{what}");
    assert_eq!(policies(headers), ["sandbox"], "{what}");
}
