//! An entry's action in words, for the Activity list (§5.8): read from
//! today's op table, tool catalog and route registry when a page is asked
//! for, so a renamed summary relabels old entries too and nothing of it is
//! stored.

use crate::openapi::Summaries;

/// The sign-in routes' events and the desktop's own actions, which no op
/// table names.
const EVENTS: &[(&str, &str)] = &[
    ("sign-in", "Signed in"),
    ("sign-in failed", "Sign-in failed"),
    ("sign-in throttled", "Sign-in held by the throttle"),
    ("sign-out", "Signed out"),
    ("password set", "Set a password through a set-password link"),
    ("password set failed", "A set-password link's submit failed"),
    ("machine_apply", "Set up this computer for lmgw"),
    ("pairing_link_open", "Opened a device's pairing link"),
];

/// `action` in words: an op's or tool's summary, a route's, an event's
/// sentence; the action itself only for a name none of them knows (an
/// entry from a version that had it).
pub fn action_label(action: &str) -> String {
    if let Some((_, l)) = EVENTS.iter().find(|(a, _)| *a == action) {
        return (*l).to_string();
    }
    let s = Summaries::get();
    if let Some(l) = s.op(action).or_else(|| s.tool(action)) {
        return l.to_string();
    }
    if let Some((method, path)) = action.split_once(' ') {
        if let Some(l) = s.of_route(method, path) {
            return l.to_string();
        }
    }
    action.to_string()
}
