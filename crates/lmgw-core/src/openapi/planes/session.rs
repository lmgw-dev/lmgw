//! `/api/session*`, the login (api-docs design §4.6), and
//! `/api/account/password`, a set-password link's submit (headless server
//! design §5.5). WP4.
//!
//! All five rows are `Cap::Public` in `CAPABILITY_TABLE` — this is the plane a
//! browser with no session talks to yet, and each handler verifies whatever
//! credential it was handed itself (`web/session.rs`'s own module doc).

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use crate::web::accounts::LinkPassword;
use crate::web::session::{LoginQuery, SignIn};
use lmgw_api_types::SessionView;

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
        errors: &[],
        example: None,
    }
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            description: "The desktop app's window opens here with ?nonce=; a browser opens it \
                with ?token= (the owner key, in the \"dashboard login\" link lmgw logs at \
                startup) or with a one-time ?nonce= from account_login_link (lmgw-server \
                login-link). Either match sets the session cookie and redirects to /; anything \
                else redirects to /?login=invalid with no cookie. With accounts on the cookie \
                is a session of the account the nonce or key belongs to, never the key. \
                Same-origin only.",
            query: Some(|g| g.root_schema_for::<LoginQuery>()),
            response: Resp::Redirect,
            ..base("GET", "/api/session/login", "Log in via a link")
        },
        DocRoute {
            description: "The current principal, as the dashboard's load check reads it, \
                whether accounts are on, and with a session its account and expiry. Never a 401, whatever credential (or none) the request carried: it answers the \
                question \"am I logged in\", and a refusal would make the answer \
                indistinguishable from a gateway that is down.",
            response: Resp::Json(|g| g.root_schema_for::<SessionView>()),
            ..base("GET", "/api/session", "Read the current session")
        },
        DocRoute {
            description: "The paste fallback {token}, and how the dashboard re-logs itself \
                after rotating owner:dashboard; with accounts on also {login, password}. A token \
                that matches no enabled owner row, or a wrong, unknown, disabled or \
                password-less sign-in name, is 401 login_invalid, not session_required. A \
                password over http from another machine is 403 login_insecure; repeated \
                failures are 429 login_throttled with Retry-After; all verification permits \
                taken is 503 login_busy with Retry-After: 1; an account changed (its password, \
                a disable, an end of its sessions) while the sign-in was checked is 409 \
                sign_in_superseded: sign in again. Bodies up to 2 MiB. Same-origin only.",
            request: Req::Json(|g| g.root_schema_for::<SignIn>()),
            response: Resp::NoContent,
            ..base("POST", "/api/session", "Sign in")
        },
        DocRoute {
            description: "Clears the session cookie and, with accounts on, ends this \
                browser's session and what it opened. Same-origin only; never refused, \
                whatever the session resolves to. A key itself is untouched.",
            response: Resp::NoContent,
            confirm_note: Some("signs this browser out of the dashboard"),
            ..base("DELETE", "/api/session", "Sign out")
        },
        DocRoute {
            description: "A set-password link's submit (headless server design §5.5): the \
                token from the link's fragment, the new password (at least 12 characters, up \
                to 2 MiB) and, for account 1's first password only, its sign-in name. Sets the \
                password, spends the link, ends the account's sessions and signs this browser \
                in. 401 link_invalid for a token unknown, used or expired; 401 \
                account_disabled for a disabled account's link, which is kept and works once \
                the account is enabled again; 403 login_insecure over http from another \
                machine; 409 accounts_off on a desktop with accounts off; 409 \
                sign_in_superseded when the account changed while its session started (the \
                password is set; sign in with it). Same-origin only.",
            request: Req::Json(|g| g.root_schema_for::<LinkPassword>()),
            response: Resp::NoContent,
            writes: Some(true),
            ..base("POST", "/api/account/password", "Set a password with a link")
        },
    ]
}
