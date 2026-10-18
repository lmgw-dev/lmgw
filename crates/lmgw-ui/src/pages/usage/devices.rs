//! The Devices card on Usage → Keys (client-apps design §1.4, §11 Q6): the
//! paired client apps, their state and policy, pairing, and the three actions
//! that end a device's connections — each confirmed with what it ends, then
//! done. And the device's level of lmgw's admin tools (L3/L5, 2026-10-07; a
//! level since the pre-merge review's P-3): off, read only or full, set on
//! the pairing form and on a device's row, confirmed with what the level
//! lets the device do whenever it is raised or set to read only.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::device_link::DevicePairingState;
use lmgw_api_types::{KeyRow, KeySaved, KeysResponse, MessageAck};
use serde_json::json;

use super::*;
use crate::widgets::{use_toasts, MenuItem, Modal, ModalFooter, RowMenu};

/// The pure decisions: where, the URL list, what to turn on, the pairing's
/// words, the certificate's notice.
mod logic;
/// What a row says under its name: pairing, notices, open links.
mod notes;
/// The pairing form.
mod pair;
/// The pairing link as a QR code.
mod qr;
/// The QR card, its confirmation, and the question closing it asks.
mod qr_card;
/// The link strip above the rows.
mod strip;
/// New token…, the re-pair with its two choices.
mod token_dialog;

pub(super) use logic::parse_urls;
use notes::PairingNotes;
use pair::PairDevice;
use qr_card::{QrCard, QrSession};
use strip::LinkStrip;
use token_dialog::TokenDialog;

/// The clock the card's relative words read (seconds since the epoch),
/// moved on by the card itself.
#[derive(Clone, Copy)]
struct Now(RwSignal<f64>);

/// What a level of lmgw's admin tools lets a device do: the words the
/// pairing form's review and the card's confirmation say it in.
fn admin_level_warning(level: &str) -> &'static str {
    match level {
        "full" => {
            "This device can then change lmgw's configuration and register programs that run on \
             this machine as the lmgw user — MCP servers with a command, containers, agents and \
             builds — through the admin tools, as well as read it. The self-admin level set in \
             Settings (Network & access → Self-admin tools) still caps it. It sees and uses the \
             Chat threads and folders that carry the self-admin toolset, in text and in voice, \
             attaches it itself and changes those threads and folders. Admin Chat stays hidden \
             from every device."
        }
        _ => {
            "This device can then read lmgw's configuration and state through the admin tools; \
             it changes nothing. It sees and uses the Chat threads and folders that carry the \
             self-admin toolset, in text and in voice, but does not attach the toolset or \
             change those threads' settings and messages or those folders' defaults. Admin Chat \
             stays hidden from every device."
        }
    }
}

/// A level in words, as the card and the review say it.
fn admin_level_words(level: &str) -> &'static str {
    match level {
        "full" => "full",
        "read_only" => "read only",
        _ => "off",
    }
}

/// Whether a key row's admin-tools level is above `off`.
fn admin_on(level: &str) -> bool {
    matches!(level, "read_only" | "full")
}

/// `device:phone` → `phone`.
fn bare(name: &str) -> &str {
    name.strip_prefix("device:").unwrap_or(name)
}

/// The two actions that end a device's connections from its menu (a new
/// token has its own dialog, with the choice of what it ends).
#[derive(Clone, Copy, PartialEq)]
enum Ending {
    Disable,
    Delete,
}

/// What only an action changes about a device row: its identity in the
/// keyed list (the live cells read the row by id).
#[derive(Clone, PartialEq, Eq, Hash)]
struct DevIdent {
    id: i64,
    name: String,
    enabled: bool,
}

/// Is this device unscoped and unbudgeted — the case §11 Q1 flags amber?
fn wide_open(r: &KeyRow) -> bool {
    every_alias(&r.scope_mode, &r.scope_patterns) && r.budget_micro <= 0
}

/// Does an alias scope let every alias through — `all`, and the scopes that
/// read the same (review W2-10): a deny list with nothing on it, an allow
/// list with a bare `*`.
fn every_alias(mode: &str, patterns: &str) -> bool {
    let mut list = patterns.lines().map(str::trim).filter(|l| !l.is_empty());
    match mode {
        "deny" => list.next().is_none(),
        "allow" => list.any(|p| p == "*"),
        _ => true,
    }
}

/// "every alias", "only chatty, gemma*", "every alias but …" — what a scope
/// lets through, in words.
fn reach(mode: &str, patterns: &str, every: &str) -> String {
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

#[component]
pub(super) fn DevicesCard(
    keys: Src<KeysResponse>,
    refresh: RwSignal<u32>,
    edit: Callback<KeyRow>,
) -> impl IntoView {
    let toasts = use_toasts();
    let pair_open = RwSignal::new(false);
    // The QR card: the link just minted, shown here and nowhere else.
    let session = RwSignal::new(None::<QrSession>);
    let card_open = RwSignal::new(false);
    // New token…, with its choices.
    let retoken = RwSignal::new(None::<KeyRow>);
    let retoken_open = RwSignal::new(false);
    // The clock the words about expiry read, moved on every 20 s.
    let now = RwSignal::new(js_sys::Date::now() / 1000.0);
    provide_context(Now(now));
    if let Ok(h) = set_interval_with_handle(
        move || now.set(js_sys::Date::now() / 1000.0),
        Duration::from_secs(20),
    ) {
        on_cleanup(move || h.clear());
    }
    let asking = RwSignal::new(None::<(Ending, KeyRow)>);
    let ask_open = RwSignal::new(false);
    let busy = RwSignal::new(false);
    // A device's admin-tools level: read only and full after a confirmation
    // that says what the level allows, off at once.
    let granting = RwSignal::new(None::<(KeyRow, &'static str)>);
    let grant_open = RwSignal::new(false);

    let idents = Memo::new(move |_| {
        keys.data.with(|d| {
            d.as_ref().map(|k| {
                k.keys
                    .iter()
                    .filter(|r| r.kind == "device")
                    .map(|r| DevIdent {
                        id: r.id,
                        name: r.name.clone(),
                        enabled: r.enabled,
                    })
                    .collect::<Vec<_>>()
            })
        })
    });

    let enable = move |r: KeyRow| {
        spawn_local(async move {
            let body = json!({ "id": r.id, "enabled": true });
            match crate::api::post::<KeySaved, _>("/api/op/key_set", &body).await {
                Ok(_) => {
                    toasts.ok(format!("device '{}' enabled", bare(&r.name)));
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let ask = Callback::new(move |(what, r): (Ending, KeyRow)| {
        asking.set(Some((what, r)));
        ask_open.set(true);
    });
    let enable = Callback::new(move |r: KeyRow| enable(r));
    let new_token = Callback::new(move |r: KeyRow| {
        retoken.set(Some(r));
        retoken_open.set(true);
    });
    let set_admin_tools = move |r: KeyRow, level: &'static str| {
        busy.set(true);
        spawn_local(async move {
            let body = json!({ "id": r.id, "self_admin": level });
            let res = crate::api::post::<KeySaved, _>("/api/op/key_set", &body).await;
            busy.set(false);
            grant_open.set(false);
            match res {
                Ok(_) => {
                    toasts.ok(if admin_on(level) {
                        format!(
                            "device '{}' uses lmgw's admin tools: {}",
                            bare(&r.name),
                            admin_level_words(level)
                        )
                    } else {
                        format!(
                            "device '{}' no longer uses lmgw's admin tools; its threads with the \
                             toolset left it",
                            bare(&r.name)
                        )
                    });
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let admin_tools = Callback::new(move |(r, level): (KeyRow, &'static str)| {
        if admin_on(level) {
            granting.set(Some((r, level)));
            grant_open.set(true);
        } else {
            set_admin_tools(r, "off");
        }
    });
    let grant = move |_| {
        if let Some((r, level)) = granting.get_untracked() {
            set_admin_tools(r, level);
        }
    };
    let confirm = move |_| {
        let Some((what, r)) = asking.get_untracked() else {
            return;
        };
        busy.set(true);
        spawn_local(async move {
            let res = match what {
                Ending::Disable => {
                    let body = json!({ "id": r.id, "enabled": false });
                    crate::api::post::<KeySaved, _>("/api/op/key_set", &body)
                        .await
                        .map(|_| ())
                }
                Ending::Delete => {
                    crate::api::post::<MessageAck, _>("/api/op/key_delete", &json!({ "id": r.id }))
                        .await
                        .map(|_| ())
                }
            };
            busy.set(false);
            ask_open.set(false);
            match res {
                Ok(()) => {
                    toasts.ok(match what {
                        Ending::Disable => format!("device '{}' disabled", bare(&r.name)),
                        Ending::Delete => format!("device '{}' deleted", bare(&r.name)),
                    });
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let body = move || match idents.get() {
        None => view! { <p class="dim">"Loading…"</p> }.into_any(),
        Some(v) if v.is_empty() => view! {
            <p class="dim wrap devices-empty">
                "No device is paired. A client app — a desktop assistant, a phone — pairs with "
                "a link that carries a key of its own: the Chat API and " <code>"/v1"</code>
                " under that key's scope and budget, and nothing of this dashboard."
            </p>
        }
        .into_any(),
        Some(_) => view! {
            // A device's scope and budget bound its own key; with *Require API
            // key* off anyone who reaches this address is served without one
            // (review W2-9).
            <Show when=move || keys.data.with(|d| d.as_ref().is_some_and(|k| !k.auth_enabled))>
                <p class="notice warn devices-auth">
                    "Require API key is off: anyone who reaches this gateway's address uses "
                    <code>"/v1"</code>
                    " without a key, whatever a device's scope and budget say. Switch it on "
                    "under " <a href="/settings">"Settings → Network & access"</a> "."
                </p>
            </Show>
            <table class="data devices-table">
                <thead>
                    <tr>
                        <th>"Device"</th>
                        <th>"State"</th>
                        <th>"Reaches"</th>
                        <th>"Spend vs budget"</th>
                        <th></th>
                    </tr>
                </thead>
                <tbody>
                    <For each=move || idents.get().unwrap_or_default() key=|d| d.clone() let:d>
                        <DeviceLine
                            ident=d
                            keys=keys
                            edit=edit
                            ask=ask
                            enable=enable
                            new_token=new_token
                            admin_tools=admin_tools
                            refresh=refresh
                        />
                    </For>
                </tbody>
            </table>
        }
        .into_any(),
    };

    view! {
        <div class="card devices-card">
            <div class="card-head">
                <span class="mini-head">"Devices"</span>
                <span class="dim devices-sub">
                    "paired client apps — the Chat API and /v1 under a key of their own"
                </span>
                <span class="spacer" style="flex:1"></span>
                <button class="btn" on:click=move |_| pair_open.set(true)>
                    "Pair a device…"
                </button>
            </div>
            <LinkStrip keys=keys refresh=refresh/>
            {body}
        </div>
        <PairDevice
            open=pair_open
            keys=keys
            refresh=refresh
            session=session
            card_open=card_open
        />
        <QrCard session=session open=card_open keys=keys refresh=refresh/>
        <TokenDialog
            target=retoken
            open=retoken_open
            refresh=refresh
            session=session
            card_open=card_open
        />
        <Modal open=grant_open title="lmgw's admin tools">
            {move || {
                granting
                    .get()
                    .map(|(r, level)| {
                        view! {
                            <p>
                                <b>
                                    {format!(
                                        "Give device '{}' lmgw's admin tools at {}?",
                                        bare(&r.name),
                                        admin_level_words(level),
                                    )}
                                </b>
                            </p>
                            <p class="notice warn">{admin_level_warning(level)}</p>
                            <p class="dim">
                                "Taking them back later stops its turns and closes its voice sessions on such threads, and they leave its lists."
                            </p>
                        }
                    })
            }}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| grant_open.set(false)>
                    "Cancel"
                </button>
                <button class="btn danger" disabled=move || busy.get() on:click=grant>
                    {move || {
                        match granting.get().map(|(_, l)| l) {
                            Some("full") => "Allow full admin tools",
                            _ => "Allow read-only admin tools",
                        }
                    }}
                </button>
            </ModalFooter>
        </Modal>
        <Modal open=ask_open title="End this device's connections">
            {move || {
                asking
                    .get()
                    .map(|(what, r)| {
                        let name = bare(&r.name).to_string();
                        let (lead, after) = match what {
                            Ending::Disable => (
                                format!("Disable device '{name}'?"),
                                "Every request it makes is refused (401 device_disabled) until \
                                 you enable it again here. Its key, policy and history stay.",
                            ),
                            Ending::Delete => (
                                format!("Delete device '{name}'?"),
                                "Its key and policy are deleted; pairing it again starts from \
                                 nothing. Its spend stays in Usage.",
                            ),
                        };
                        view! {
                            <p><b>{lead}</b></p>
                            <p class="notice warn">
                                "Its Chat feed, voice sessions and tool link close now."
                            </p>
                            <p class="dim">{after}</p>
                        }
                    })
            }}
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| ask_open.set(false)>
                    "Cancel"
                </button>
                <button class="btn danger" disabled=move || busy.get() on:click=confirm>
                    {move || match asking.get().map(|(w, _)| w) {
                        Some(Ending::Disable) => "Disable the device",
                        _ => "Delete the device",
                    }}
                </button>
            </ModalFooter>
        </Modal>
    }
}

/// One device row: built once per [`DevIdent`], its cells read live.
#[component]
fn DeviceLine(
    ident: DevIdent,
    keys: Src<KeysResponse>,
    edit: Callback<KeyRow>,
    ask: Callback<(Ending, KeyRow)>,
    enable: Callback<KeyRow>,
    new_token: Callback<KeyRow>,
    admin_tools: Callback<(KeyRow, &'static str)>,
    refresh: RwSignal<u32>,
) -> impl IntoView {
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
    let state = move || {
        let Some(r) = row.get() else {
            return ().into_any();
        };
        if !r.online.is_empty() {
            return view! {
                <span class="dev-online" title="its open connections now">
                    {format!("online ({})", online_text(&r))}
                </span>
            }
            .into_any();
        }
        let seen = r
            .last_seen_at
            .as_deref()
            .map(|t| js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_str(t)).get_time())
            .filter(|ms| !ms.is_nan());
        match seen {
            Some(ms) => view! {
                <span class="dim" title=crate::fmt::local_datetime(ms / 1000.0)>
                    {format!("last seen {}", crate::fmt::rel_time(ms / 1000.0))}
                </span>
            }
            .into_any(),
            None => view! { <span class="dim">"never connected"</span> }.into_any(),
        }
    };
    let reaches = move || {
        row.with(|r| {
            r.as_ref()
                .map(|r| {
                    let mut s = format!(
                        "{} · {}",
                        reach(&r.scope_mode, &r.scope_patterns, "alias"),
                        reach(&r.tool_scope_mode, &r.tool_scope_patterns, "tool")
                    );
                    if !r.device_tools.is_empty() {
                        s.push_str(&format!(" · device tools: {}", r.device_tools.join(", ")));
                    }
                    s
                })
                .unwrap_or_default()
        })
    };
    // Its device-tools grant (client-apps design L16 as changed
    // 2026-10-15), beside the name: each other device's tools it may use, a
    // label no paired device hosts now in amber.
    let granted = move || {
        let Some(r) = row.get() else {
            return Vec::new();
        };
        let hosted = keys.data.with(|d| {
            d.as_ref()
                .map(|k| super::device_tools::hosted_labels(&k.keys))
                .unwrap_or_default()
        });
        r.device_tools
            .iter()
            .map(|l| {
                let host = hosted
                    .iter()
                    .find(|h| h.label.eq_ignore_ascii_case(l))
                    .map(|h| h.device.clone());
                (l.clone(), host)
            })
            .collect::<Vec<_>>()
    };
    let unmet = move || {
        let Some(r) = row.get() else {
            return Vec::new();
        };
        let hosted = keys.data.with(|d| {
            d.as_ref()
                .map(|k| super::device_tools::hosted_labels(&k.keys))
                .unwrap_or_default()
        });
        super::device_tools::needs_grant(
            &r.tool_scope_mode,
            &r.tool_scope_patterns,
            &r.device_tools,
            Some(r.id),
            &hosted,
        )
    };
    let granted_chips = move || {
        granted()
            .into_iter()
            .map(|(label, host)| match host {
                Some(device) => view! {
                    " "
                    <span
                        class="chip info key-kind mono-sm"
                        title=format!("may use the tools device '{device}' hosts, named {label}__…, as its tool scope picks them")
                    >
                        {format!("uses {label}__")}
                    </span>
                }
                .into_any(),
                None => view! {
                    " "
                    <span
                        class="chip warn key-kind mono-sm"
                        title="granted, but no paired device hosts tools under this label now: the grant waits for one that does"
                    >
                        {format!("{label}__: no device hosts it")}
                    </span>
                }
                .into_any(),
            })
            .collect_view()
    };
    let spend = move || {
        row.with(|r| {
            let Some(r) = r else { return String::new() };
            let cur = cur.get();
            if r.budget_micro > 0 {
                format!(
                    "{} of {} / {}",
                    money(r.spent_micro, &cur),
                    money(r.budget_micro, &cur),
                    r.budget_period
                )
            } else if r.spent_micro > 0 {
                format!("{} spent · no budget", money(r.spent_micro, &cur))
            } else {
                "no budget".to_string()
            }
        })
    };
    // The hosting grant (§1.5), where the name is: the prefix its own
    // tools are named under.
    let hosts = move || {
        row.get()
            .and_then(|r| r.hosts_label.map(|l| (l, r.reaches)))
            .map(|(l, who)| {
                view! {
                    " "
                    <span
                        class="chip info key-kind mono-sm"
                        title=format!(
                            "may host its own MCP tools, named {l}__… — reached by: {}",
                            who.join(", ")
                        )
                    >
                        {format!("{l}__")}
                    </span>
                }
            })
    };
    // Who reaches its hosted tools (§5.6): the owner, the device, each key
    // and device whose device-tools grant names the label, and each agent
    // whose manifest does.
    let reached_by = move || {
        row.get()
            .filter(|r| r.hosts_label.is_some() && !r.reaches.is_empty())
            .map(|r| {
                let who = r.reaches.join(", ");
                let title = who.clone();
                view! {
                    <div class="mini-note dim" title=title>
                        "its tools are reached by: "
                        {who}
                    </div>
                }
            })
    };
    // Its level of lmgw's admin tools, beside the name.
    let admin_flag = move || {
        row.get().filter(|r| admin_on(r.self_admin.as_str())).map(|r| {
            let level = admin_level_words(r.self_admin.as_str());
            view! {
                " "
                <span
                    class="chip warn key-kind"
                    title=format!("lmgw's admin tools at {level}: the Chat threads and folders with the self-admin toolset; the self-admin level set in Settings caps it")
                >
                    {format!("admin tools: {level}")}
                </span>
            }
        })
    };
    let open_flag = move || {
        row.get().filter(wide_open).map(|_| {
            view! {
                " "
                <span
                    class="chip warn key-kind"
                    title="Every alias and no budget: a stolen device key spends on every alias without a ceiling. Set a budget where a cloud alias is reachable (Edit)."
                >
                    "every alias · no budget"
                </span>
            }
        })
    };
    let items = Signal::derive(move || {
        let Some(r) = row.get() else {
            return Vec::new();
        };
        let (r1, r2, r3) = (r.clone(), r.clone(), r.clone());
        // Every level but the one it has: read only and full confirmed.
        let tools: Vec<MenuItem> = [
            (
                "read_only",
                "Admin tools: read only…",
                "read lmgw's configuration and state through the admin tools",
            ),
            (
                "full",
                "Admin tools: full…",
                "change lmgw's configuration, and register programs that run on this machine",
            ),
            (
                "off",
                "Take back the admin tools",
                "its turns stop and its threads and folders with the self-admin toolset leave it now",
            ),
        ]
        .into_iter()
        .filter(|(level, _, _)| *level != r.self_admin.as_str() && (admin_on(level) || admin_on(r.self_admin.as_str())))
        .map(|(level, label, title)| {
            let r = r.clone();
            MenuItem::new(label, move || admin_tools.run((r.clone(), level))).title(title)
        })
        .collect();
        let toggle = if r.enabled {
            MenuItem::new("Disable…", move || ask.run((Ending::Disable, r1.clone())))
                .title("refuse every request it makes, and close its connections now")
        } else {
            MenuItem::new("Enable", move || enable.run(r1.clone()))
                .title("let it in again with the key it has")
        };
        let mut items = vec![
            MenuItem::new("New token…", move || new_token.run(r2.clone()))
                .title("pair it again: a new link, keeping its current key until the new one is confirmed, or ending it now"),
            toggle,
        ];
        items.extend(tools);
        items.push(
            MenuItem::new("Delete…", move || ask.run((Ending::Delete, r3.clone())))
                .title("delete the key and its policy"),
        );
        items
    });
    // A pairing from before TLS: its old key opens nothing (§12).
    let v1_flag = move || {
        row.with(|r| {
            r.as_ref()
                .and_then(|r| r.pairing.as_ref())
                .is_some_and(|p| p.state == DevicePairingState::V1)
        })
        .then(|| {
            view! {
                " "
                <span
                    class="chip warn key-kind"
                    title="Paired with a link from before TLS: its old key opens nothing. New token… pairs it again."
                >
                    "v1: pair again"
                </span>
            }
        })
    };
    let bare_name = bare(&ident.name).to_string();
    view! {
        <tr class="dev-sum">
            <td>
                <span class="mono-sm">
                    <span class="pfx">"device:"</span>
                    {bare_name}
                </span>
                {(!ident.enabled)
                    .then(|| view! { " " <span class="chip off key-kind">"disabled"</span> })}
                {hosts}
                {granted_chips}
                {move || super::device_tools::needs_grant_chips(unmet())}
                {admin_flag}
                {open_flag}
                {v1_flag}
            </td>
            <td class="nowrap">{state}</td>
            <td class="wrap dev-reach dim">{reaches}</td>
            <td class="wrap dev-spend">{spend}</td>
            <td class="actions">
                <div class="row-acts key-acts">
                    <RowMenu items=items/>
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
        // What the row says about the device, under its summary and across
        // every column, so the notes wrap in the card's width and never sit
        // on the other cells.
        <tr class="dev-detail">
            <td colspan="5">
                {reached_by}
                <PairingNotes id=id row=row refresh=refresh/>
            </td>
        </tr>
    }
}

/// The kinds a device has open, each with its count when more than one is
/// ("feed ×3, voice"): a client that keeps opening links shows (review
/// W4-25).
fn online_text(r: &KeyRow) -> String {
    r.online
        .iter()
        .map(|kind| {
            match r
                .open_links
                .iter()
                .find(|l| &l.kind == kind)
                .map(|l| l.count)
            {
                Some(n) if n > 1 => format!("{kind} ×{n}"),
                _ => kind.clone(),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_online_line_counts_a_kind_held_more_than_once() {
        let r = KeyRow {
            online: vec!["feed".into(), "voice".into()],
            open_links: vec![
                lmgw_api_types::OpenLinks {
                    kind: "feed".into(),
                    count: 3,
                    ..Default::default()
                },
                lmgw_api_types::OpenLinks {
                    kind: "voice".into(),
                    count: 1,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(online_text(&r), "feed ×3, voice");
        // A gateway that sends no counts: the kinds alone.
        let r = KeyRow {
            online: vec!["tools".into()],
            ..Default::default()
        };
        assert_eq!(online_text(&r), "tools");
    }

    #[test]
    fn every_scope_that_lets_every_alias_through_reads_as_all() {
        assert!(every_alias("all", ""));
        assert!(every_alias("deny", ""));
        assert!(every_alias("allow", "chatty\n*"));
        assert!(!every_alias("deny", "claude-*"));
        assert!(!every_alias("allow", "chatty\ngemma*"));
        assert!(!every_alias("allow", ""));
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
