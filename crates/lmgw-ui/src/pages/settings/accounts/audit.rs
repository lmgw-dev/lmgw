//! The Activity list on the Accounts card (headless server design §5.8,
//! ruling 18's follow-up): the audit log a page at a time, newest first,
//! everyone's, one account's or the entries of no account, with the whole
//! log's size and its retention. Kept and readable while accounts are off
//! on the desktop.

use leptos::prelude::*;
use lmgw_api_types::{AccountList, AuditEntry, AuditPage};
use serde_json::{json, Value};

use super::super::Page;
use crate::widgets::Select;

/// Entries on a page: the card says it, beside the buttons.
const PAGE: u32 = 50;

/// The picker's value for every entry and for those of no account.
const EVERYONE: &str = "everyone";
const NO_ACCOUNT: &str = "none";

/// A dotted settings name in words, for a field no row of this page shows:
/// its parts, spaced, in order ("Hold › active").
fn spelled(path: &str) -> String {
    let words: Vec<String> = path
        .split('.')
        .map(|p| p.replace(['_', '-'], " "))
        .collect();
    let mut s = words.join(" › ");
    if let Some(first) = s.get(..1) {
        s = first.to_uppercase() + &s[1..];
    }
    s
}

/// A changed setting by its row's label on this page ("Session lifetime");
/// a member of a row's value (a forge token's host) by the row's label with
/// the member; else its name in words, never the dotted key.
fn field_label(path: &str) -> String {
    let row = |k: &str| super::super::fields().find(|d| d.key == k).map(|d| d.label);
    if let Some(l) = row(path) {
        return l.to_string();
    }
    let mut head = path;
    while let Some((h, _)) = head.rsplit_once('.') {
        if let Some(l) = row(h) {
            return format!("{l} ({})", &path[h.len() + 1..]);
        }
        head = h;
    }
    spelled(path)
}

/// A target's key in words.
fn key_label(k: &str) -> String {
    match k {
        "login" => "sign-in name".into(),
        "display_name" => "display name".into(),
        "account" | "account_id" => "account".into(),
        "key_id" => "key".into(),
        "model_id" => "model".into(),
        "upstream_id" => "upstream".into(),
        "agent_id" => "agent".into(),
        "server" => "MCP server".into(),
        "repo" => "repository".into(),
        "with" => "signed in with".into(),
        other => other.replace('_', " "),
    }
}

/// How an entry reached the gateway, in words.
fn via_label(via: &str) -> String {
    match via {
        "dashboard" => "the dashboard".into(),
        "api" => "an API key".into(),
        "mcp" => "the admin MCP endpoint".into(),
        "chat" => "a model in the Chat".into(),
        "agent" => "an agent's run".into(),
        "cli" => "the command line".into(),
        "cli-offline" => "the command line, gateway stopped".into(),
        "tray" => "the tray menu".into(),
        "app" => "the lmgw window".into(),
        other => other.into(),
    }
}

/// A target as one line: `name: spike`, a settings save's fields as
/// `Session lifetime = 7`.
fn target_text(t: &Value) -> String {
    let plain = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let Value::Object(map) = t else {
        return String::new();
    };
    map.iter()
        .map(|(k, v)| match (k.as_str(), v) {
            ("fields", Value::Object(fields)) if fields.is_empty() => "no fields".to_string(),
            ("fields", Value::Object(fields)) => fields
                .iter()
                .map(|(f, v)| format!("{} = {}", field_label(f), plain(v)))
                .collect::<Vec<_>>()
                .join(", "),
            _ => format!("{}: {}", key_label(k), plain(v)),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The whole log in one sentence.
fn summary(p: &AuditPage) -> String {
    let kept = if p.keep_days == 0 {
        "every entry is kept".to_string()
    } else {
        format!("entries are kept {} days", p.keep_days)
    };
    let oldest = p
        .oldest
        .as_deref()
        .map(|t| format!(", the oldest from {}", super::when(t)))
        .unwrap_or_default();
    format!(
        "{} entries, {} on disk{oldest}; {kept} (Keep audit entries, above). Who it names is \
         who signed in, not proof of who acted: every account can set another's password.",
        p.total,
        lmgw_api_types::units::fmt_bytes(p.bytes),
    )
}

/// Where the list stands: counted from the page actually shown, never a
/// computed range that new entries would shift.
fn position(page_no: usize) -> String {
    if page_no <= 1 {
        format!("the newest {PAGE}")
    } else {
        format!("page {page_no}, {PAGE} per page")
    }
}

/// The picker's choices: everyone, each account, and no account.
fn who_options(list: Option<&AccountList>) -> Vec<(String, String)> {
    let mut out = vec![(EVERYONE.to_string(), "Everyone".to_string())];
    for a in list.map(|l| l.accounts.as_slice()).unwrap_or_default() {
        let name = if a.display_name.is_empty() || a.display_name == a.login {
            a.login.clone()
        } else {
            format!("{} ({})", a.display_name, a.login)
        };
        out.push((a.id.to_string(), name));
    }
    out.push((
        NO_ACCOUNT.to_string(),
        "No account (failed sign-ins, the command line with the gateway stopped)".to_string(),
    ));
    out
}

/// `audit_list`'s `account` for the picker's value.
fn account_arg(who: &str) -> Value {
    match who.parse::<i64>() {
        Ok(id) => json!(id),
        Err(_) => json!(who),
    }
}

#[component]
pub(in crate::pages::settings) fn AuditPanel(
    page: Page,
    id: String,
    hidden: Signal<bool>,
) -> impl IntoView {
    let who = RwSignal::new(EVERYONE.to_string());
    // The ids each page starts after, newest page first: `None` is the
    // newest entries. A new pick starts at the newest again.
    let cursors = RwSignal::new(vec![None::<i64>]);
    Effect::new(move |_| {
        who.track();
        cursors.set(vec![None]);
    });
    let accounts = LocalResource::new(move || {
        page.data.with(|_| ());
        async move {
            super::op::<AccountList>("account_list", json!({}))
                .await
                .ok()
        }
    });
    let options = Signal::derive(move || {
        let list = accounts.get().flatten();
        who_options(list.as_ref())
    });
    let list = LocalResource::new(move || {
        // Read again whenever the settings answer is (a save, a switch).
        page.data.with(|_| ());
        let before = cursors.with(|c| c.last().copied().flatten());
        let account = account_arg(&who.get());
        async move {
            super::op::<AuditPage>(
                "audit_list",
                json!({ "account": account, "before": before, "count": PAGE }),
            )
            .await
        }
    });
    view! {
        <div class="set-about set-audit" id=id hidden=move || hidden.get()>
            <div class="row" style="gap:8px;align-items:center;margin-bottom:6px">
                <span>"Whose entries"</span>
                <Select value=who options=options/>
            </div>
            <Suspense fallback=|| view! { <span class="dim">"…"</span> }>
                {move || {
                    list.get().map(|r| match r {
                        Err(e) => view! { <div class="notice err">{e.to_string()}</div> }.into_any(),
                        Ok(p) => view! { <Shown p=p cursors=cursors/> }.into_any(),
                    })
                }}
            </Suspense>
        </div>
    }
}

/// One answered page: the failed-writes notice, the log's sentence, the
/// entries and the buttons.
#[component]
fn Shown(p: AuditPage, cursors: RwSignal<Vec<Option<i64>>>) -> impl IntoView {
    let failed = (p.failed_writes > 0).then(|| {
        view! {
            <div class="notice warn">
                {format!(
                    "{} audit write(s) failed since the gateway started; the requests were \
                     answered, and each failure is an error line in the log.",
                    p.failed_writes,
                )}
            </div>
        }
    });
    let shown = position(cursors.with(Vec::len));
    let last_id = p.entries.last().map(|e| e.id);
    let more = p.more;
    let summary = summary(&p);
    let body = if p.entries.is_empty() {
        view! {
            <p class="dim">"no entries here yet: the log is written while accounts are on"</p>
        }
        .into_any()
    } else {
        view! { <Entries entries=p.entries/> }.into_any()
    };
    view! {
        {failed}
        <p class="dim">{summary}</p>
        {body}
        <div class="row" style="gap:8px;margin-top:8px;align-items:center">
            <button
                class="btn"
                disabled=move || cursors.with(|c| c.len() <= 1)
                on:click=move |_| cursors.update(|c| {
                    c.pop();
                })
            >
                "Newer"
            </button>
            <button
                class="btn"
                disabled=!more
                on:click=move |_| cursors.update(|c| c.push(last_id))
            >
                "Older"
            </button>
            <span class="dim">{shown}</span>
        </div>
    }
}

/// A collapsed entry's count and last time, under its first time.
fn repeats_text(e: &AuditEntry) -> Option<String> {
    match (&e.last_ts, e.count) {
        (Some(last), n) if n > 1 => Some(format!("×{n}, the last {}", super::when(last))),
        _ => None,
    }
}

/// One page's table: who with how and from where under it, the action
/// with what it names under it, so a page fits the card's width and no
/// target is cut.
#[component]
fn Entries(entries: Vec<AuditEntry>) -> impl IntoView {
    view! {
        <table class="data">
            <thead>
                <tr>
                    <th class="nowrap">"When"</th>
                    <th>"Who"</th>
                    <th>"What"</th>
                    <th class="num">"Answer"</th>
                </tr>
            </thead>
            <tbody>
                {entries
                    .into_iter()
                    .map(|e| {
                        let what = target_text(&e.target);
                        let how = format!("{} from {}", via_label(&e.via), e.peer);
                        let when = super::when(&e.ts);
                        let repeats = repeats_text(&e);
                        view! {
                            <tr>
                                <td class="nowrap">
                                    {when}
                                    {repeats.map(|r| view! { <div class="dim">{r}</div> })}
                                </td>
                                <td>
                                    {e.principal}
                                    <div class="dim mono-sm">{how}</div>
                                </td>
                                <td>
                                    <span>{e.action_label}</span>
                                    {(!what.is_empty())
                                        .then(|| view! { <div class="dim">{what}</div> })}
                                </td>
                                <td class="num">{e.outcome}</td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_without_a_row_is_spelled_out_never_dotted() {
        assert_eq!(field_label("accounts.session_days"), "Session lifetime");
        let l = field_label("hold.active");
        assert!(!l.contains('.') && !l.contains('_'), "{l}");
        assert_eq!(spelled("hold.active"), "Hold › active");
        assert_eq!(
            via_label("cli-offline"),
            "the command line, gateway stopped"
        );
        assert_eq!(position(1), "the newest 50");
        assert_eq!(position(3), "page 3, 50 per page");
        assert_eq!(account_arg("7"), json!(7));
        assert_eq!(account_arg(NO_ACCOUNT), json!("none"));
    }
}
