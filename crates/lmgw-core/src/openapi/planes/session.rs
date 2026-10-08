//! `/api/session*`, the login (api-docs design §4.6). WP4.
//!
//! All four rows are `Cap::Public` in `CAPABILITY_TABLE` — this is the plane a
//! browser with no session talks to yet, and each handler verifies whatever
//! credential it was handed itself (`web/session.rs`'s own module doc).

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use crate::web::session::{LoginQuery, SessionView, SignIn};

fn base(method: &'static str, path: &'static str, summary: &'static str) -> DocRoute {
    DocRoute {
        method,
        path,
        tag: "session",
        summary,
        description: "",
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::Untyped("BUG: session.rs route builder did not override the response"),
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        example: None,
    }
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            description: "The desktop app's window opens here with ?nonce=; a browser opens it \
                with ?token= (the owner key, in the \"dashboard login\" link lmgw logs at \
                startup). Either match sets the session cookie and redirects to /; anything \
                else redirects to /?login=invalid with no cookie. Same-origin only.",
            query: Some(|g| g.root_schema_for::<LoginQuery>()),
            response: Resp::Redirect,
            ..base("GET", "/api/session/login", "Log in via a link")
        },
        DocRoute {
            description: "The current principal, as the dashboard's load check reads it. \
                Never a 401, whatever credential (or none) the request carried: it answers the \
                question \"am I logged in\", and a refusal would make the answer \
                indistinguishable from a gateway that is down.",
            response: Resp::Json(|g| g.root_schema_for::<SessionView>()),
            ..base("GET", "/api/session", "Read the current session")
        },
        DocRoute {
            description: "The paste fallback, and how the dashboard re-logs itself after \
                rotating owner:dashboard. A token that matches no enabled owner row is 401 \
                login_invalid, not session_required. Same-origin only.",
            request: Req::Json(|g| g.root_schema_for::<SignIn>()),
            response: Resp::NoContent,
            ..base("POST", "/api/session", "Sign in")
        },
        DocRoute {
            description: "Clears the session cookie. Same-origin only; never refused, \
                whatever the session resolves to. The key itself is untouched.",
            response: Resp::NoContent,
            confirm_note: Some("signs this browser out of the dashboard"),
            ..base("DELETE", "/api/session", "Sign out")
        },
    ]
}
