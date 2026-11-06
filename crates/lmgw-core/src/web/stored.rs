//! The two headers that keep anything a user, a model, an upload, a peer or
//! an upstream put into lmgw from running script at the gateway's origin
//! (machine-check design D25, WP13, D90).
//!
//! `X-Content-Type-Options: nosniff` keeps a browser to the type the route
//! names, and `Content-Security-Policy: sandbox` gives the document an opaque
//! origin (no scripts, no same-origin reads of the dashboard's session) even
//! when it is opened in a window at `http://127.0.0.1:8001/…`. A navigation
//! carries the owner's cookie, so a route that needs `Inference`, `Chat` or
//! `Admin` is no protection by itself: the cookie is the owner, and with
//! **Require API key** off nobody needs one for `/v1`.
//!
//! **Where it is applied.** Every answer of the gateway's own origin gets
//! both, from the router-wide layer in `server::build_router_for` ([`by_default`]),
//! which sits outside the gate, so its refusals carry them too. The one
//! exemption is the dashboard itself: `ui::routes` marks every answer it gives
//! [`DashboardOwned`] ([`dashboard_owned`]), and those keep their own policy
//! (`ui::DASHBOARD_CSP`) and no `nosniff`. An agent's origin (`<id>.localhost`)
//! is answered above the layer and is the container's own.
//!
//! The routes that serve stored content call [`sandboxed`] on their own answer
//! as well, and [`super::ranged`] on every answer it builds (200, 206, 416), so
//! a stored file is covered where it is served and not only by the router's
//! default. Both say the same and never twice: a bare `sandbox` already on the
//! answer is not added again.

use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
use axum::response::Response;

/// `X-Content-Type-Options`.
const NOSNIFF: HeaderName = HeaderName::from_static("x-content-type-options");

/// The mark on every answer the dashboard's own routes give (`ui::routes`):
/// the SPA's `index.html`, its Trunk-built assets, its client-side routes'
/// fallback, the old `/ui` mount's redirect, and that router's own 404, 503
/// and refusals. [`by_default`] leaves those as they are.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DashboardOwned;

/// Set both headers on `headers`.
///
/// `nosniff` is inserted (it has no other value). The policy is **appended**
/// unless one of the answer's policies already is a bare `sandbox`: a
/// container's or an upstream's own `Content-Security-Policy` stays, and
/// every policy a response carries is enforced, so the sandbox holds whatever
/// the other one says — `sandbox allow-scripts allow-same-origin` included,
/// which is a sandbox with the two doors open and does not count.
pub(crate) fn sandbox_headers(headers: &mut HeaderMap) {
    headers.insert(NOSNIFF, HeaderValue::from_static("nosniff"));
    if !has_bare_sandbox(headers) {
        headers.append(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox"),
        );
    }
}

/// Whether one of `headers`' policies has a `sandbox` directive with no
/// flags. A header value may carry several policies (`,`) of several
/// directives (`;`).
fn has_bare_sandbox(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::CONTENT_SECURITY_POLICY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| {
            v.split([',', ';'])
                .any(|d| d.trim().eq_ignore_ascii_case("sandbox"))
        })
}

/// `resp` with both headers, whatever its status.
pub(crate) fn sandboxed(mut resp: Response) -> Response {
    sandbox_headers(resp.headers_mut());
    resp
}

/// The router-wide default (D90): both headers on every answer but the
/// dashboard's own ([`DashboardOwned`]).
pub(crate) async fn by_default(resp: Response) -> Response {
    if resp.extensions().get::<DashboardOwned>().is_some() {
        return resp;
    }
    sandboxed(resp)
}

/// `ui::routes`' layer: marks the answer as the dashboard's own.
pub(crate) async fn dashboard_owned(mut resp: Response) -> Response {
    resp.extensions_mut().insert(DashboardOwned);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csp(headers: &HeaderMap) -> Vec<&str> {
        headers
            .get_all(header::CONTENT_SECURITY_POLICY)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect()
    }

    #[test]
    fn an_answer_without_a_policy_gets_the_sandbox_once_however_often_it_is_applied() {
        let mut h = HeaderMap::new();
        sandbox_headers(&mut h);
        sandbox_headers(&mut h);
        assert_eq!(csp(&h), ["sandbox"]);
        assert_eq!(h.get_all(NOSNIFF).iter().count(), 1);
        assert_eq!(h[NOSNIFF], "nosniff");
    }

    #[test]
    fn another_policy_stays_and_the_sandbox_is_a_second_one() {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'self'; script-src 'unsafe-inline'"),
        );
        h.insert(NOSNIFF, HeaderValue::from_static("bogus"));
        sandbox_headers(&mut h);
        assert_eq!(
            csp(&h),
            ["default-src 'self'; script-src 'unsafe-inline'", "sandbox"]
        );
        assert_eq!(h[NOSNIFF], "nosniff");
    }

    #[test]
    fn only_a_bare_sandbox_counts_as_one() {
        for (theirs, added) in [
            ("sandbox allow-scripts allow-same-origin", true),
            ("default-src 'none'; script-src sandbox", true),
            ("default-src 'none'; Sandbox ", false),
            ("default-src 'none', sandbox", false),
        ] {
            let mut h = HeaderMap::new();
            h.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(theirs),
            );
            sandbox_headers(&mut h);
            let want: &[&str] = if added {
                &[theirs, "sandbox"]
            } else {
                &[theirs]
            };
            assert_eq!(csp(&h), want, "{theirs}");
        }
    }
}
