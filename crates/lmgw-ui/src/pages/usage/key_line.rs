//! What a key says about itself, in the words the list row, the hero and
//! the detail's groups share: its kind, its name without the kind's prefix,
//! its spend against its budget (with the meter), what its scope lets
//! through, when it was last used, and whether revoking it is the owner's
//! call.

use leptos::prelude::*;
use lmgw_api_types::KeyRow;

use crate::fmt::{compact, grouped};
use crate::widgets::Ico;

use super::*;

/// The kinds in the list's order: (kind, heading, icon).
pub(super) const KINDS: &[(&str, &str, Ico)] = &[
    ("owner", "Owner keys", Ico::Key),
    ("key", "Client keys", Ico::Key),
    ("device", "Devices", Ico::Globe),
    ("agent", "Agent tokens", Ico::Bot),
    ("internal", "Internal identities", Ico::Wrench),
];

/// A kind's icon.
pub(super) fn kind_icon(kind: &str) -> Ico {
    KINDS
        .iter()
        .find(|(k, _, _)| *k == kind)
        .map_or(Ico::Key, |(_, _, i)| *i)
}

/// The heading a kind's rows sit under.
pub(super) fn kind_heading(kind: &str) -> &'static str {
    KINDS
        .iter()
        .find(|(k, _, _)| *k == kind)
        .map_or("Other keys", |(_, h, _)| *h)
}

/// A kind in a few words, as the hero says it under the name.
pub(super) fn kind_word(kind: &str) -> &'static str {
    match kind {
        "owner" => "Owner key",
        "device" => "Paired device",
        "agent" => "Agent token",
        "internal" => "Internal identity",
        _ => "Client key",
    }
}

/// `owner:dashboard` → `dashboard`: the server names a key by what it is
/// (`internal:`, `owner:`, `agent:`, `device:`), and the kind is said
/// beside it. A client key's own name is kept whole, colon and all.
pub(super) fn bare_name<'a>(kind: &str, name: &'a str) -> &'a str {
    match name.split_once(':') {
        Some((pfx, rest)) if pfx == kind && kind != "key" => rest,
        _ => name,
    }
}

/// Spend against the key's budget, the overshoot stated rather than clamped:
/// cost is only knowable after a response, so one request always crosses
/// the line (design §4.3).
pub(super) fn spend_text(r: &KeyRow, cur: &str) -> String {
    if r.budget_micro <= 0 {
        if r.spent_micro > 0 {
            format!("{} spent · no budget", money(r.spent_micro, cur))
        } else {
            "no budget".to_string()
        }
    } else if r.spent_micro > r.budget_micro {
        format!(
            "{} of {} — over by {}",
            money(r.spent_micro, cur),
            money(r.budget_micro, cur),
            money(r.spent_micro - r.budget_micro, cur),
        )
    } else {
        format!(
            "{} of {} / {}",
            money(r.spent_micro, cur),
            money(r.budget_micro, cur),
            r.budget_period
        )
    }
}

/// Is the key past its budget?
pub(super) fn over_budget(r: &KeyRow) -> bool {
    r.budget_micro > 0 && r.spent_micro > r.budget_micro
}

/// The spend meter and its line. Same obligation as every other money total
/// on the page: say what the figure does not cover. A meter that is 39 %
/// full of a number with a hole in it is not 39 % full.
#[component]
pub(super) fn SpendMeter(
    #[prop(into)] row: Signal<Option<KeyRow>>,
    #[prop(into)] cur: Signal<String>,
) -> impl IntoView {
    move || {
        let r = row.get()?;
        let cur = cur.get();
        let ratio = if r.budget_micro > 0 {
            r.spent_micro as f64 / r.budget_micro as f64
        } else {
            0.0
        };
        let cls = if r.budget_micro <= 0 {
            "meter"
        } else if ratio > 1.0 {
            "meter over"
        } else if ratio > 0.8 {
            "meter warn"
        } else {
            "meter"
        };
        let rem = unpriced_note(r.spent_unknown_requests, r.spent_unknown_tokens).map(|n| {
            view! {
                <span
                    class="dim"
                    title="These requests spent money the gateway could not price, so their cost is missing from the figure beside them — unknown, not zero. Some count in it with a lower bound: one unpriced only for a hosted tool's fee with what its tokens cost, an OpenAI answer at a tier above Standard with no price for it at the standard price, and a stream cut before its usage came with a local count of its text."
                >
                    {format!(" · {n}")}
                </span>
            }
        });
        let over = over_budget(&r);
        Some(view! {
            <div class="key-spend-line">
                {(r.budget_micro > 0)
                    .then(|| {
                        view! {
                            <div class=cls>
                                <i style=format!("width:{:.1}%", (ratio * 100.0).min(100.0))></i>
                            </div>
                        }
                    })}
                <span class="key-note" class:over=over>
                    {spend_text(&r, &cur)}
                    {rem}
                </span>
            </div>
        })
    }
}

/// "every alias", "only chatty, gemma*", "every alias but …" — what a scope
/// lets through, in words.
pub(super) fn reach(mode: &str, patterns: &str, every: &str) -> String {
    let list = patterns
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    match mode {
        "allow" if list.is_empty() => "none".to_string(),
        "allow" => format!("only {list}"),
        "deny" if list.is_empty() => format!("every {every}"),
        "deny" => format!("every {every} but {list}"),
        _ => format!("every {every}"),
    }
}

/// What a key may reach, in one line: its aliases, for a client key or a
/// device its tools when they narrow, and the paired devices' tools it may
/// use. An owner key reaches everything.
pub(super) fn scope_text(r: &KeyRow) -> String {
    if r.kind == "owner" {
        return "every capability".to_string();
    }
    let mut s = reach(&r.scope_mode, &r.scope_patterns, "alias");
    let client = matches!(r.kind.as_str(), "key" | "device");
    if client && r.tool_scope_mode != "all" && !r.tool_scope_mode.is_empty() {
        s.push_str(&format!(
            " · {}",
            reach(&r.tool_scope_mode, &r.tool_scope_patterns, "tool")
        ));
    }
    if client && !r.device_tools.is_empty() {
        s.push_str(&format!(" · device tools {}", r.device_tools.join(", ")));
    }
    s
}

/// The rate limits in one line: "60 requests / min · 2 at once", or none.
pub(super) fn limits_text(r: &KeyRow) -> String {
    let mut parts = Vec::new();
    if r.rpm_limit > 0 {
        parts.push(format!("{} requests / min", grouped(r.rpm_limit as u64)));
    }
    if r.tpm_limit > 0 {
        parts.push(format!("{} tokens / min", compact(r.tpm_limit as f64)));
    }
    if r.concurrency_limit > 0 {
        parts.push(format!("{} at once", r.concurrency_limit));
    }
    if parts.is_empty() {
        "no rate limit".to_string()
    } else {
        parts.join(" · ")
    }
}

/// A rollup hour key (`2026-09-19T19`, UTC) as unix seconds.
pub(super) fn hour_key_unix(k: &str) -> Option<f64> {
    let head = k.get(0..13)?;
    let d = js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_str(&format!(
        "{head}:00:00Z"
    )));
    let ms = d.get_time();
    (!ms.is_nan()).then_some(ms / 1000.0)
}

/// When a key was last used, as the rollup knows it: to the hour.
/// "used 5d ago", "used this hour", "never used".
pub(super) fn used_words(k: Option<&str>) -> String {
    let Some(t) = k.and_then(hour_key_unix) else {
        return match k {
            Some(k) => format!("used {k}"),
            None => "never used".to_string(),
        };
    };
    let now = js_sys::Date::now() / 1000.0;
    if now - t < 3600.0 {
        "used this hour".to_string()
    } else {
        format!("used {}", crate::fmt::rel_time(t))
    }
}

/// The hour of the last use in local time, for a tooltip.
pub(super) fn used_tip(k: Option<&str>) -> String {
    k.and_then(hour_key_unix)
        .map(|t| {
            format!(
                "in the hour from {} (local time)",
                crate::fmt::local_datetime(t)
            )
        })
        .unwrap_or_else(|| "no request recorded".to_string())
}

/// What a key's request count covers: its own budget period.
pub(super) fn period_tip(period: &str) -> &'static str {
    match period {
        "day" => "Requests today",
        "total" => "Requests in all time",
        _ => "Requests this month",
    }
}

/// What a kind of key is, in a sentence.
pub(super) fn kind_tip(kind: &str) -> &'static str {
    match kind {
        "internal" => "An internal identity: budgetable, and can never authenticate anything",
        // An owner key is the dashboard's own session and the self-admin
        // credential (principals §3.1).
        "owner" => "An owner credential: holds every capability, and is the only kind a browser session is accepted for",
        "agent" => "An agent's own token: its scope and enabled flag follow the agent",
        "device" => "A paired client app's key: the Chat API and /v1 under its own scope and budget",
        _ => "A client key: what a caller presents on /v1, under its scope and budget",
    }
}

/// Is revoking this row the owner's call? An `internal:*` identity carries
/// the budget and scope policy of a gateway consumer (it dies with the row),
/// an `agent:*` token is the agent's own credential (its app and runs start
/// failing — rotate or revoke it on the agent's page), and the dashboard key
/// is the door the server refuses to delete (principals §3.12). A device is
/// deleted with its own question, which says what it ends.
pub(super) fn revocable(r: &KeyRow) -> bool {
    !matches!(r.kind.as_str(), "internal" | "agent" | "device")
        && r.name != "owner:dashboard"
        // An account's sign-in key goes with its account (headless server
        // design §5.1).
        && r.door_of.is_none()
}

/// Another account's sign-in key: changed through that account, so neither
/// copied nor rotated here (headless server design §5.1).
pub(super) fn others_door(r: &KeyRow) -> bool {
    r.door_of.is_some() && !r.yours
}

/// Is `when` — a `YYYY-MM-DD` (the end of that day) or RFC 3339 timestamp —
/// already past? `false` for empty or unreadable text, which the server
/// refuses or ignores itself.
pub(super) fn expiry_past(when: &str) -> bool {
    let when = when.trim();
    if when.is_empty() {
        return false;
    }
    let at = js_sys::Date::parse(when);
    if at.is_nan() {
        return false;
    }
    // A bare date is read as its midnight (UTC); the key works the whole day.
    let end = if when.len() == 10 {
        at + 86_399_000.0
    } else {
        at
    };
    end <= js_sys::Date::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, name: &str) -> KeyRow {
        KeyRow {
            kind: kind.into(),
            name: name.into(),
            enabled: true,
            scope_mode: "all".into(),
            tool_scope_mode: "all".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_name_loses_its_kinds_prefix_only() {
        assert_eq!(bare_name("owner", "owner:dashboard"), "dashboard");
        assert_eq!(bare_name("device", "device:phone"), "phone");
        assert_eq!(bare_name("key", "ci:runner"), "ci:runner");
        assert_eq!(bare_name("key", "laptop"), "laptop");
    }

    #[test]
    fn the_scope_line_says_what_narrows() {
        let mut r = row("key", "a");
        assert_eq!(scope_text(&r), "every alias");
        r.scope_mode = "allow".into();
        r.scope_patterns = "chatty\ngemma*".into();
        r.tool_scope_mode = "deny".into();
        r.tool_scope_patterns = "github__*".into();
        r.device_tools = vec!["kai".into()];
        assert_eq!(
            scope_text(&r),
            "only chatty, gemma* · every tool but github__* · device tools kai"
        );
        // An internal identity's tool scope binds nothing it presents.
        r.kind = "internal".into();
        assert_eq!(scope_text(&r), "only chatty, gemma*");
        assert_eq!(scope_text(&row("owner", "owner:x")), "every capability");
    }

    #[test]
    fn limits_read_as_words_or_none() {
        let mut r = row("key", "a");
        assert_eq!(limits_text(&r), "no rate limit");
        r.rpm_limit = 60;
        r.concurrency_limit = 2;
        assert_eq!(limits_text(&r), "60 requests / min · 2 at once");
    }

    #[test]
    fn revoking_is_the_owners_call_only_for_client_and_owner_keys() {
        assert!(revocable(&row("key", "a")));
        assert!(revocable(&row("owner", "owner:mcp")));
        assert!(!revocable(&row("owner", "owner:dashboard")));
        assert!(!revocable(&row("agent", "agent:x")));
        assert!(!revocable(&row("internal", "internal:x")));
        assert!(!revocable(&row("device", "device:x")));
        let mut door = row("owner", "owner:alice");
        door.door_of = Some("alice".into());
        assert!(!revocable(&door));
        assert!(others_door(&door));
        door.yours = true;
        assert!(!others_door(&door));
    }

    #[test]
    fn a_scope_reads_as_what_it_lets_through() {
        assert_eq!(reach("all", "", "alias"), "every alias");
        assert_eq!(
            reach("allow", "chatty\ngemma*", "alias"),
            "only chatty, gemma*"
        );
        assert_eq!(
            reach("deny", "github__*", "tool"),
            "every tool but github__*"
        );
        assert_eq!(reach("allow", "", "tool"), "none");
    }
}
