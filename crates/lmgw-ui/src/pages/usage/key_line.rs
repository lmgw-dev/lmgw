use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{KeyRow, KeysResponse};
use serde_json::{json, Value};

use crate::fmt::{compact, grouped};
use crate::widgets::{use_toasts, ConfirmButton, MenuItem, RowMenu};

use super::*;

/// What only an edit changes about a key row: its identity in the keyed
/// list.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct KeyIdent {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) enabled: bool,
    pub(super) revocable: bool,
}

/// One key row: built once per [`KeyIdent`], its figures read live.
#[component]
pub(super) fn KeyLine(
    ident: KeyIdent,
    keys: Src<KeysResponse>,
    revoke: Callback<(i64, String)>,
    edit: Callback<KeyRow>,
    rotate: Callback<KeyRow>,
) -> impl IntoView {
    let toasts = use_toasts();
    let id = ident.id;
    let row = Memo::new(move |_| {
        keys.data.with(|d| {
            d.as_ref()
                .and_then(|k| k.keys.iter().find(|r| r.id == id).cloned())
        })
    });
    let cur = Memo::new(move |_| {
        keys.data
            .with(|d| d.as_ref().map(|k| k.currency.clone()).unwrap_or_default())
    });
    let get = move |f: fn(&KeyRow) -> String| row.with(|r| r.as_ref().map(f).unwrap_or_default());

    // The overshoot is stated, not clamped: cost is only knowable after a
    // response, so one request always crosses the line (design §4.3). Same
    // obligation as every other money total on the page: say what the figure
    // does not cover. A meter that is 39% full of a number with a hole in it
    // is not 39% full.
    let spend = move || {
        let Some(r) = row.get() else {
            return ().into_any();
        };
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
                    title="These requests spent money the gateway could not price, so they are missing from the figure beside them — unknown, not zero."
                >
                    {format!(" · {n}")}
                </span>
            }
        });
        let note = if r.budget_micro <= 0 {
            view! {
                <span class="dim">
                    {if r.spent_micro > 0 {
                        format!("{} spent · no budget", money(r.spent_micro, &cur))
                    } else {
                        "no budget".to_string()
                    }}
                </span>
                {rem}
            }
            .into_any()
        } else if r.spent_micro > r.budget_micro {
            view! {
                <span style="color:var(--err)">
                    {format!(
                        "{} of {} — over by {}",
                        money(r.spent_micro, &cur),
                        money(r.budget_micro, &cur),
                        money(r.spent_micro - r.budget_micro, &cur),
                    )}
                </span>
                {rem}
            }
            .into_any()
        } else {
            view! {
                {money(r.spent_micro, &cur)}
                <span class="dim">
                    {format!(" of {} / {}", money(r.budget_micro, &cur), r.budget_period)}
                </span>
                {rem}
            }
            .into_any()
        };
        view! {
            <div class="split-bar">
                {(r.budget_micro > 0)
                    .then(|| {
                        view! {
                            <div class=cls>
                                <i style=format!("width:{:.1}%", (ratio * 100.0).min(100.0))></i>
                            </div>
                        }
                    })}
                <span class="key-note">{note}</span>
            </div>
        }
        .into_any()
    };
    // The cell clips on a narrow window; its tooltip says it whole.
    let spend_tip = move || {
        let Some(r) = row.get() else {
            return String::new();
        };
        let cur = cur.get();
        let mut t = if r.budget_micro <= 0 {
            if r.spent_micro > 0 {
                format!("{} spent · no budget", money(r.spent_micro, &cur))
            } else {
                "no budget".to_string()
            }
        } else {
            format!(
                "{} of {} / {}",
                money(r.spent_micro, &cur),
                money(r.budget_micro, &cur),
                r.budget_period
            )
        };
        if let Some(n) = unpriced_note(r.spent_unknown_requests, r.spent_unknown_tokens) {
            t.push_str(&format!(" · {n}"));
        }
        t
    };
    // The tool scope rides along only when it narrows anything: "all tools"
    // on every row would be noise, and only a client key's is ever read.
    let scope = move || {
        get(|r| {
            let mut s = if r.scope_mode == "all" {
                "all aliases".to_string()
            } else {
                format!("{} {}", r.scope_mode, r.scope_patterns.replace('\n', ", "))
            };
            if r.kind == "key" && r.tool_scope_mode != "all" && !r.tool_scope_mode.is_empty() {
                s.push_str(&format!(
                    " · tools {} {}",
                    r.tool_scope_mode,
                    r.tool_scope_patterns.replace('\n', ", ")
                ));
            }
            s
        })
    };
    // What a narrow table drops (rpm, tpm, last used) is in the name's
    // tooltip — on the link itself, whose own title would otherwise win over
    // the cell's on every row (review par:PAR-5) — with the kind, and what a
    // click does.
    let name_tip = {
        let kind = kind_tip(&ident.kind);
        let name = ident.name.clone();
        move || {
            let limits = row.with(|r| {
                r.as_ref().map(|r| {
                    format!(
                        "last used {} · rpm {} · tpm {}",
                        r.last_used
                            .as_deref()
                            .and_then(hour_key_unix)
                            .map(|t| format!("in the hour from {}", crate::fmt::local_datetime(t)))
                            .unwrap_or_else(|| "never".into()),
                        if r.rpm_limit > 0 {
                            r.rpm_limit.to_string()
                        } else {
                            "—".into()
                        },
                        if r.tpm_limit > 0 {
                            compact(r.tpm_limit as f64)
                        } else {
                            "—".into()
                        },
                    )
                })
            });
            let mut t = name.clone();
            if let Some(k) = kind {
                t.push_str(&format!(" — {k}"));
            }
            if let Some(l) = limits {
                t.push_str(&format!("\n{l}"));
            }
            t.push_str("\nClick: this key's requests in Traffic");
            t
        }
    };
    // Copy is a fetch, not a stored string: the plaintext is asked for on
    // click and lives for as long as it takes to paste it, exactly as the
    // agent page's *Copy token* does.
    let copy = move || {
        spawn_local(async move {
            let res =
                crate::api::post::<Value, _>("/api/op/key_reveal", &json!({ "id": id })).await;
            match res {
                Ok(v) => {
                    let key = v.get("key").and_then(Value::as_str).unwrap_or_default();
                    crate::widgets::copy_secret(key, toasts, "key copied".into());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let owner = ident.kind == "owner";
    let rname = ident.name.clone();
    let label = match (kind_tip(&ident.kind), ident.name.split_once(':')) {
        // The server names a key by what it is (`internal:`, `owner:`,
        // `agent:`); the prefix is the kind, dimmed.
        (Some(_), Some((pfx, rest))) if pfx == ident.kind => view! {
            <span class="pfx">{format!("{pfx}:")}</span>
            {rest.to_string()}
        }
        .into_any(),
        _ => ident.name.clone().into_any(),
    };
    view! {
        <tr>
            <td>
                // An internal identity cannot authenticate anything, but it
                // spends and it is logged — "what has the corpus ingest been
                // doing" is one of the better uses of this link.
                <a
                    class="mono-sm key-link"
                    href=TrafficLink::scoped("", "").key(id).href()
                    title=name_tip
                >
                    {label}
                </a>
                {(!ident.enabled)
                    .then(|| {
                        view! {
                            " "
                            <span class="chip off key-kind">"disabled"</span>
                        }
                    })}
            </td>
            <td class="clip key-scope mono-sm dim" title=scope>
                {scope}
            </td>
            <td class="clip key-spend" title=spend_tip>
                {spend}
            </td>
            <td class="num col-p2">
                {move || get(|r| if r.rpm_limit > 0 { r.rpm_limit.to_string() } else { "—".into() })}
            </td>
            <td class="num col-p2">
                {move || get(|r| if r.tpm_limit > 0 { compact(r.tpm_limit as f64) } else { "—".into() })}
            </td>
            <td class="num" title=move || get(|r| period_tip(&r.budget_period))>
                {move || get(|r| grouped(r.requests.max(0) as u64))}
            </td>
            <td class="num dim col-p3">
                {move || row.with(|r| last_used(r.as_ref().and_then(|r| r.last_used.as_deref())))}
            </td>
            // One group, the same slots on every row: Edit (the row's action)
            // at the right edge, then the owner key's menu, then Revoke apart
            // at the left.
            <td class="actions">
                <div class="row-acts key-acts">
                    {ident
                        .revocable
                        .then(|| {
                            let rname = rname.clone();
                            view! {
                                <ConfirmButton
                                    label="Revoke"
                                    confirm=format!("Revoke {rname}?")
                                    title="Delete this key: anything presenting it is refused from now on"
                                    on_confirm=Callback::new(move |()| revoke.run((id, rname.clone())))
                                />
                            }
                        })}
                    // An owner key's own two: stored, so copyable;
                    // replaceable, behind a question.
                    {if owner {
                        let items = vec![
                            MenuItem::new("Copy key", copy)
                                .title("copy this key to the clipboard — it is stored, not shown once"),
                            MenuItem::new(
                                    "Rotate…",
                                    move || {
                                        if let Some(r) = row.get_untracked() {
                                            rotate.run(r);
                                        }
                                    },
                                )
                                .title("replace this key; anything holding the old one is locked out at once"),
                        ];
                        view! { <RowMenu items=Signal::stored(items)/> }.into_any()
                    } else {
                        view! { <span class="menu-slot" aria-hidden="true"></span> }.into_any()
                    }}
                    <button
                        class="btn ghost sm"
                        on:click=move |_| {
                            if let Some(r) = row.get_untracked() {
                                edit.run(r);
                            }
                        }
                    >
                        "Edit"
                    </button>
                </div>
            </td>
        </tr>
    }
}

pub(super) const KEY_COLS: u32 = 8;

/// A rollup hour key (`2026-09-19T19`, UTC) as unix seconds.
fn hour_key_unix(k: &str) -> Option<f64> {
    let head = k.get(0..13)?;
    let d = js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_str(&format!(
        "{head}:00:00Z"
    )));
    let ms = d.get_time();
    (!ms.is_nan()).then_some(ms / 1000.0)
}

/// When a key was last used, as the rollup knows it: to the hour. "5d ago",
/// with the local hour in the tooltip.
fn last_used(k: Option<&str>) -> AnyView {
    let Some(t) = k.and_then(hour_key_unix) else {
        return view! { <span>{k.unwrap_or("never").to_string()}</span> }.into_any();
    };
    let now = js_sys::Date::now() / 1000.0;
    let text = if now - t < 3600.0 {
        "this hour".to_string()
    } else {
        crate::fmt::rel_time(t)
    };
    view! {
        <time title=format!("in the hour from {} (local time)", crate::fmt::local_datetime(t))>
            {text}
        </time>
    }
    .into_any()
}

/// What a key's request count covers: its own budget period.
fn period_tip(period: &str) -> String {
    match period {
        "day" => "requests today (this key's budget period is a day)".into(),
        "total" => "requests in all time (this key's budget is a lifetime total)".into(),
        _ => "requests this month (this key's budget period)".into(),
    }
}

/// What a named kind of key is, for the dimmed name prefix; `None` for a
/// client key, which has no prefix.
fn kind_tip(kind: &str) -> Option<&'static str> {
    match kind {
        "internal" => Some("an internal identity: budgetable, and can never authenticate anything"),
        // An owner key is the dashboard's own session and the self-admin
        // credential (principals §3.1).
        "owner" => Some(
            "an owner credential: holds every capability, and is the only kind a browser session is accepted for",
        ),
        "agent" => Some("an agent's own token: its scope and enabled flag follow the agent"),
        _ => None,
    }
}

/// Is revoking this row the owner's call? An `internal:*` identity carries
/// the budget and scope policy of a gateway consumer (it dies with the row),
/// an `agent:*` token is the agent's own credential (its app and runs start
/// failing — rotate or revoke it on the agent's page), and the dashboard key
/// is the door the server refuses to delete (principals §3.12).
pub(super) fn revocable(r: &KeyRow) -> bool {
    !matches!(r.kind.as_str(), "internal" | "agent") && r.name != "owner:dashboard"
}
