//! The router-wide nosniff + sandbox default (machine-check design WP13,
//! D90), walked over the capability table.
//!
//! Every row but the dashboard's own answers with `X-Content-Type-Options:
//! nosniff` and exactly one `Content-Security-Policy: sandbox`, whatever its
//! status: a stored file, a relayed upstream answer, a JSON list, a refusal,
//! a redirect, a stream's head, a WebSocket route asked without the upgrade.
//! The dashboard (`web::ui::routes`) keeps its own policy and no sandbox,
//! and an agent origin is answered above the layer.

use std::time::Duration;

use lmgw_core::server::CAPABILITY_TABLE;
use lmgw_core::state::AppState;
use serde_json::json;

use crate::common;
use crate::support::stored::{assert_sandboxed, bare_sandboxes};

/// The capture every `{..}` segment is asked with: an id nothing has, so the
/// handlers behind it answer their own not-found.
const PROBE: &str = "sandbox-walk-probe";

/// `web::ui::routes`, the one exemption: the page, its assets and the
/// client-side routes' fallback, and the old `/ui` mount's redirect.
const DASHBOARD_ROWS: [(&str, &str); 5] = [
    ("GET", "/"),
    ("GET", "/ui"),
    ("GET", "/ui/"),
    ("GET", "/ui/{*path}"),
    ("GET", "/{*path}"),
];

/// The dashboard's own policy (`web::ui::DASHBOARD_CSP`), as the page sends it.
const DASHBOARD_CSP: &str = "img-src 'self' data: blob:; media-src 'self' data: blob:; \
                             font-src 'self' data:; style-src 'self' 'unsafe-inline'";

/// One capture value per `{..}` segment; `{job_id}` is an `i64` in its
/// handler's extractor.
fn fill(path: &str) -> String {
    path.split('/')
        .map(|seg| match seg.strip_prefix('{') {
            Some(name) if name.contains("job") => "1".to_string(),
            Some(_) => PROBE.to_string(),
            None => seg.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[tokio::test]
async fn every_row_but_the_dashboards_own_sends_nosniff_and_the_sandbox() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state).await;
    // The owner's bearer on every request, no redirect followed (the login
    // route's 302 is what is asked about), and no cookie jar.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let mut walked = 0;
    for (method, path, cap) in CAPABILITY_TABLE {
        if DASHBOARD_ROWS.contains(&(*method, *path)) {
            continue;
        }
        let url = format!("{gw}{}", fill(path));
        // `*` is an `any(..)` route (the agent proxy's faces): asked as a GET,
        // which is what a navigation is.
        let verb = if *method == "*" { "GET" } else { method };
        let mut req = http
            .request(verb.parse().unwrap(), &url)
            .bearer_auth(&gw.key)
            // `/mcp*` answers 406 to a client that does not accept JSON; a
            // 406 would carry both headers too, but the walk means the route.
            .header("accept", "application/json");
        if verb == "POST" || verb == "PUT" {
            req = if *path == "/api/session" {
                req.json(&json!({ "token": gw.key }))
            } else {
                req.json(&json!({}))
            };
        }
        // Only the head is read: a feed, `/api/events` or `GET /mcp` is a
        // stream that would hold the walk for as long as it lasts.
        let resp = req
            .send()
            .await
            .unwrap_or_else(|e| panic!("{verb} {url}: {e}"));
        let status = resp.status().as_u16();
        assert_sandboxed(
            resp.headers(),
            &format!("{method} {path} ({cap:?}) answered {status}"),
        );
        walked += 1;
    }
    assert_eq!(
        walked,
        CAPABILITY_TABLE.len() - DASHBOARD_ROWS.len(),
        "a dashboard row in this walk is no longer in the table"
    );
}

/// The exemption: the page keeps its own policy and gets neither header, and
/// so do its assets and its client-side routes.
#[tokio::test]
async fn the_dashboards_own_answers_keep_their_policy_and_no_sandbox() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state).await;
    let c = gw.client_no_redirect();

    for path in ["/", "/chat", "/agents/board"] {
        let r = c.get(format!("{gw}{path}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let policies: Vec<_> = r
            .headers()
            .get_all("content-security-policy")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(policies, [DASHBOARD_CSP], "{path}");
        assert!(
            r.headers().get("x-content-type-options").is_none(),
            "{path}"
        );
        assert!(
            r.text().await.unwrap().contains("<!DOCTYPE html>"),
            "{path}: not the SPA"
        );
    }

    // A Trunk asset (no policy at all: it is not a document) and the old
    // mount's redirect.
    for path in ["/vendor/highlight.js", "/ui/chat"] {
        let r = c.get(format!("{gw}{path}")).send().await.unwrap();
        assert!(
            r.status().is_success() || r.status().is_redirection(),
            "{path}: {}",
            r.status()
        );
        assert_eq!(bare_sandboxes(r.headers()), 0, "{path}");
        assert!(
            r.headers().get("x-content-type-options").is_none(),
            "{path}"
        );
    }
}

/// An agent origin is answered by the `Host` dispatch, above the layer: what
/// it sends is the container's (or, for a label nothing answers on, the
/// dispatch's own 404), never the gateway's sandbox.
#[tokio::test]
async fn an_agent_origin_is_answered_above_the_layer() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state).await;
    let host = "nobody.localhost";
    let r = gw
        .origin_client(&[host])
        .get(format!("{}/", gw.origin(host)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(bare_sandboxes(r.headers()), 0, "{:?}", r.headers());
    assert!(r.headers().get("x-content-type-options").is_none());
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["code"], "not_found", "{body}");
}
