//! The Devices card on Usage → Keys (client-apps design §1.4, §11 Q6): the
//! paired client apps, their state and policy, pairing, and the three actions
//! that end a device's connections — each confirmed with what it ends, then
//! done. And the device's level of lmgw's admin tools (L3/L5, 2026-10-07; a
//! level since the pre-merge review's P-3): off, read only or full, set on
//! the pairing form and on a device's row, confirmed with what the level
//! lets the device do whenever it is raised or set to read only.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{ConnectInfo, KeyRow, KeysResponse, ToolInventory};
use serde_json::{json, Value};

use super::scope_editor::{AliasScopeEditor, ScopeState, ToolScopeEditor};
use super::*;
use crate::widgets::{copy_secret, use_toasts, MenuItem, Modal, ModalFooter, RowMenu, Select};

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

/// A pairing link just minted (pair or rotate): shown once, then gone.
#[derive(Clone)]
pub(super) struct Minted {
    name: String,
    link: String,
    url: String,
    url_note: Option<String>,
    rotated: bool,
}

impl Minted {
    fn from_answer(v: &Value, rotated: bool) -> Self {
        let text = |f: &str| {
            v.get(f)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Self {
            name: bare(&text("name")).to_string(),
            link: text("link"),
            url: text("url"),
            url_note: v
                .get("url_note")
                .and_then(Value::as_str)
                .map(str::to_string),
            rotated,
        }
    }
}

/// `device:phone` → `phone`.
fn bare(name: &str) -> &str {
    name.strip_prefix("device:").unwrap_or(name)
}

/// The three actions that end a device's connections.
#[derive(Clone, Copy, PartialEq)]
enum Ending {
    Disable,
    Rotate,
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
    let minted = RwSignal::new(None::<Minted>);
    let minted_open = RwSignal::new(false);
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
            match crate::api::post::<Value, _>("/api/op/key_set", &body).await {
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
    let set_admin_tools = move |r: KeyRow, level: &'static str| {
        busy.set(true);
        spawn_local(async move {
            let body = json!({ "id": r.id, "self_admin": level });
            let res = crate::api::post::<Value, _>("/api/op/key_set", &body).await;
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
            let (op, body) = match what {
                Ending::Disable => ("key_set", json!({ "id": r.id, "enabled": false })),
                Ending::Rotate => ("key_rotate", json!({ "id": r.id })),
                Ending::Delete => ("key_delete", json!({ "id": r.id })),
            };
            let res = crate::api::post::<Value, _>(&format!("/api/op/{op}"), &body).await;
            busy.set(false);
            ask_open.set(false);
            match res {
                Ok(v) => {
                    if what == Ending::Rotate {
                        minted.set(Some(Minted::from_answer(&v, true)));
                        minted_open.set(true);
                    }
                    toasts.ok(match what {
                        Ending::Disable => format!("device '{}' disabled", bare(&r.name)),
                        Ending::Rotate => format!("device '{}' rotated", bare(&r.name)),
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
                            admin_tools=admin_tools
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
            {body}
        </div>
        <PairDevice open=pair_open keys=keys refresh=refresh minted=minted minted_open=minted_open/>
        <LinkShown open=minted_open minted=minted/>
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
                            Ending::Rotate if !r.enabled => (
                                format!("Rotate device '{name}'?"),
                                "Its key is replaced at once; the old one matches nothing from \
                                 now on. The new pairing link is shown next, once — but the \
                                 device is disabled, so the new key is refused too until you \
                                 enable it here. Its policy and history stay.",
                            ),
                            Ending::Rotate => (
                                format!("Rotate device '{name}'?"),
                                "Its key is replaced at once; the old one matches nothing from \
                                 now on. The new pairing link is shown next, once — the device \
                                 must be paired again with it. Its policy and history stay.",
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
                        Some(Ending::Rotate) => "Rotate and show the new link",
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
    admin_tools: Callback<(KeyRow, &'static str)>,
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
                    format!(
                        "{} · {}",
                        reach(&r.scope_mode, &r.scope_patterns, "alias"),
                        reach(&r.tool_scope_mode, &r.tool_scope_patterns, "tool")
                    )
                })
                .unwrap_or_default()
        })
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
        row.get().and_then(|r| r.hosts_label).map(|l| {
            view! {
                " "
                <span
                    class="chip info key-kind mono-sm"
                    title=format!("may host its own MCP tools, named {l}__…")
                >
                    {format!("{l}__")}
                </span>
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
            MenuItem::new("Rotate…", move || ask.run((Ending::Rotate, r2.clone())))
                .title("a new key and pairing link; the device must be paired again"),
            toggle,
        ];
        items.extend(tools);
        items.push(
            MenuItem::new("Delete…", move || ask.run((Ending::Delete, r3.clone())))
                .title("delete the key and its policy"),
        );
        items
    });
    let bare_name = bare(&ident.name).to_string();
    view! {
        <tr>
            <td>
                <span class="mono-sm">
                    <span class="pfx">"device:"</span>
                    {bare_name}
                </span>
                {(!ident.enabled)
                    .then(|| view! { " " <span class="chip off key-kind">"disabled"</span> })}
                {hosts}
                {admin_flag}
                {open_flag}
            </td>
            <td class="nowrap">{state}</td>
            <td class="clip dim" title=reaches>{reaches}</td>
            <td class="clip" title=spend>{spend}</td>
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
    }
}

/// Is `url` this computer only? The note §1.4 puts beside such an address.
/// An IP is read as one (`127.example.net` is a name, not loopback; review
/// W2-21); a name only when it is `localhost`.
fn loopback(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

/// "Pair a device": a name, the address, the scopes and the budget — shown,
/// prefilled `all` and no budget (§11 Q1) — and an optional hosting label.
/// The form is confirmed on a review step that restates what the device will
/// reach, then the key is minted and its link shown once.
#[component]
fn PairDevice(
    open: RwSignal<bool>,
    keys: Src<KeysResponse>,
    refresh: RwSignal<u32>,
    minted: RwSignal<Option<Minted>>,
    minted_open: RwSignal<bool>,
) -> impl IntoView {
    let toasts = use_toasts();
    let reviewing = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let url = RwSignal::new(String::new());
    let label = RwSignal::new(String::new());
    // The device's level of lmgw's admin tools: off unless chosen.
    let admin_tools = RwSignal::new("off".to_string());
    let budget = RwSignal::new(String::new());
    let period = RwSignal::new("month".to_string());
    let busy = RwSignal::new(false);
    let scope = ScopeState::new();
    let page = crate::scope::Scope::new();

    // The addresses this gateway answers on, for the prefill and the list.
    let connect = LocalResource::new(move || {
        let wanted = open.get();
        async move {
            if !wanted {
                return None;
            }
            crate::api::get::<ConnectInfo>("/api/connect").await.ok()
        }
    });
    let bases = Memo::new(move |_| {
        connect
            .get()
            .flatten()
            .map(|c| c.bases.into_iter().map(|b| b.url).collect::<Vec<_>>())
            .unwrap_or_default()
    });
    Effect::new(move |_| {
        if let Some(first) = bases.get().first() {
            if url.get_untracked().trim().is_empty() {
                url.set(first.clone());
            }
        }
    });
    let inventory = LocalResource::new(move || {
        let wanted = open.get();
        async move {
            if !wanted {
                return None;
            }
            Some(
                crate::api::get::<ToolInventory>("/api/tools")
                    .await
                    .map_err(|e| e.to_string()),
            )
        }
    });
    let inv_state: Signal<Option<Result<ToolInventory, String>>> =
        Signal::derive(move || inventory.get().flatten());

    let currency = Memo::new(move |_| {
        keys.data
            .with(|d| d.as_ref().map(|k| k.currency.clone()).unwrap_or_default())
    });
    // The budget as typed, read the way the policy dialog reads it: `None`
    // when it does not parse, which is refused out loud.
    let budget_micro = Memo::new(move |_| {
        let raw = budget.get().trim().replace(',', ".");
        if raw.is_empty() {
            return Some(0);
        }
        raw.parse::<f64>()
            .ok()
            .filter(|v| *v >= 0.0)
            .map(|v| (v * 1e6).round() as i64)
    });
    let aliases = move || reach(&scope.scope_mode.get(), &scope.alias_patterns(), "alias");
    let tools = move || reach(&scope.tool_mode.get(), &scope.tool_patterns(), "tool");
    let budget_words = move || match budget_micro.get() {
        Some(0) | None => "none".to_string(),
        Some(m) => format!(
            "{} per {}",
            money(m, &currency.get()),
            match period.get().as_str() {
                "day" => "day",
                "total" => "lifetime (total)",
                _ => "month",
            }
        ),
    };
    let wide = move || {
        every_alias(&scope.scope_mode.get(), &scope.alias_patterns())
            && budget_micro.get() == Some(0)
    };
    // Require API key off and an address beyond this computer (review W2-9):
    // anyone who reaches it is served without a key, and a Delete or Rotate
    // of a stolen device leaves its key working as no key at all.
    let open_door = move || {
        keys.data
            .with(|d| d.as_ref().is_some_and(|k| !k.auth_enabled))
            && !loopback(&url.get())
    };

    let reset = move || {
        reviewing.set(false);
        name.set(String::new());
        label.set(String::new());
        admin_tools.set("off".into());
        budget.set(String::new());
        period.set("month".into());
        scope.load("all", "", "all", "");
    };
    let review = move |_| {
        if name.get_untracked().trim().is_empty() {
            toasts
                .err("name the device first — it is how its card and its voice takeovers name it");
            return;
        }
        if budget_micro.get_untracked().is_none() {
            toasts.err("budget must be an amount like 10 or 10.50 — empty means none");
            return;
        }
        if scope.tool_mode.get_untracked() != "all" && scope.groups.get_untracked().is_none() {
            toasts.err("the tool list is still loading — try again in a moment");
            return;
        }
        reviewing.set(true);
    };
    let pair = move |_| {
        if busy.get_untracked() {
            return;
        }
        let mut body = json!({
            "kind": "device",
            "name": name.get_untracked().trim(),
            "url": url.get_untracked().trim(),
            "scope_mode": scope.scope_mode.get_untracked(),
            "scope_patterns": untrack(|| scope.alias_patterns()),
            "tool_scope_mode": scope.tool_mode.get_untracked(),
            "tool_scope_patterns": untrack(|| scope.tool_patterns()),
            "budget_micro": budget_micro.get_untracked().unwrap_or(0),
            "budget_period": period.get_untracked(),
        });
        let l = label.get_untracked().trim().to_string();
        if !l.is_empty() {
            body["hosts_label"] = json!(l);
        }
        // Confirmed in the review step, which says what the level lets the
        // device do.
        let level = admin_tools.get_untracked();
        if admin_on(&level) {
            body["self_admin"] = json!(level);
        }
        busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/key_create", &body).await;
            if !page.alive() {
                if res.is_ok() {
                    toasts.warn(
                        "the device was paired, but this page was left before its link could be \
                         shown — rotate it on the Devices card",
                    );
                }
                return;
            }
            busy.set(false);
            match res {
                Ok(v) => {
                    minted.set(Some(Minted::from_answer(&v, false)));
                    open.set(false);
                    minted_open.set(true);
                    reset();
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => {
                    reviewing.set(false);
                    toasts.err(e.to_string());
                }
            }
        });
    };
    let period_opts = Signal::derive(|| {
        vec![
            ("day".to_string(), "per day".to_string()),
            ("month".into(), "per month".into()),
            ("total".into(), "total".into()),
        ]
    });
    // Off first, then read only: the first choice above off.
    let admin_opts = Signal::derive(|| {
        vec![
            ("off".to_string(), "off".to_string()),
            ("read_only".into(), "read only".into()),
            ("full".into(), "full".into()),
        ]
    });

    view! {
        <Modal open=open title="Pair a device" guard=true>
            <div class="form pair-form">
                <Show
                    when=move || reviewing.get()
                    fallback=move || {
                        view! {
                            <div class="row">
                                <span class="lbl">"name"</span>
                                <input
                                    class="input"
                                    placeholder="desktop, phone, …"
                                    aria-label="Device name"
                                    prop:value=move || name.get()
                                    on:input=move |ev| name.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="row" style="margin-top:10px">
                                <span class="lbl">"address"</span>
                                <input
                                    class="input mono"
                                    style="flex:1"
                                    list="pair-bases"
                                    aria-label="The address the device dials"
                                    prop:value=move || url.get()
                                    on:input=move |ev| url.set(event_target_value(&ev))
                                />
                                <datalist id="pair-bases">
                                    <For each=move || bases.get() key=|u| u.clone() let:u>
                                        <option value=u></option>
                                    </For>
                                </datalist>
                            </div>
                            <div class="mini-note dim">
                                {move || {
                                    if loopback(&url.get()) {
                                        "Reachable from this computer only — a phone needs the address it reaches this gateway by (a LAN address, a tunnel's name)."
                                    } else {
                                        "The address the device dials; it goes into the link."
                                    }
                                }}
                            </div>
                            <AliasScopeEditor state=scope disabled=Signal::derive(|| false)/>
                            <ToolScopeEditor state=scope inventory=inv_state disabled=Signal::derive(|| false)/>
                            <div class="row" style="margin-top:10px">
                                <span class="lbl">"budget"</span>
                                <input
                                    class="input mono"
                                    style="max-width:110px"
                                    placeholder="none"
                                    prop:value=move || budget.get()
                                    on:input=move |ev| budget.set(event_target_value(&ev))
                                />
                                <Select value=period options=period_opts/>
                            </div>
                            <div class="mini-note dim">
                                "Set a budget where a cloud alias is in reach: a device key that leaks spends until something stops it."
                            </div>
                            <div class="row" style="margin-top:10px">
                                <span class="lbl">"hosts tools as"</span>
                                <input
                                    class="input mono"
                                    style="max-width:180px"
                                    placeholder="optional label"
                                    aria-label="Hosting label"
                                    prop:value=move || label.get()
                                    on:input=move |ev| label.set(event_target_value(&ev))
                                />
                                <span class="dim mini-note">
                                    "the prefix its own MCP tools are named under (label__…)"
                                </span>
                            </div>
                            <div class="row" style="margin-top:10px">
                                <span class="lbl">"admin tools"</span>
                                <Select value=admin_tools options=admin_opts/>
                                <span class="dim mini-note">
                                    "off by default; the review says what a level allows"
                                </span>
                            </div>
                        }
                    }
                >
                    <p>
                        "Pair device " <b>{move || format!("'{}'", name.get().trim())}</b>
                        " with this reach:"
                    </p>
                    <table class="pair-review">
                        <tbody>
                            <tr><th>"Aliases"</th><td>{aliases}</td></tr>
                            <tr><th>"Tools"</th><td>{tools}</td></tr>
                            <tr><th>"Budget"</th><td>{budget_words}</td></tr>
                            <tr>
                                <th>"Hosts tools as"</th>
                                <td class="mono-sm">
                                    {move || {
                                        let l = label.get();
                                        if l.trim().is_empty() { "—".to_string() } else { l.trim().to_string() }
                                    }}
                                </td>
                            </tr>
                            <tr>
                                <th>"Admin tools"</th>
                                <td>
                                    {move || admin_level_words(&admin_tools.get())}
                                </td>
                            </tr>
                            <tr>
                                <th>"Address"</th>
                                <td>
                                    <span class="mono-sm">{move || url.get()}</span>
                                    {move || {
                                        loopback(&url.get())
                                            .then_some(" — reachable from this computer only")
                                    }}
                                </td>
                            </tr>
                        </tbody>
                    </table>
                    <Show when=wide>
                        <p class="notice warn">
                            "Every alias and no budget: a stolen key from this device spends on every alias without a ceiling. Go back and set a budget if a cloud alias is reachable."
                        </p>
                    </Show>
                    <Show when=open_door>
                        <p class="notice warn">
                            "Require API key is off: anyone who reaches this address uses /v1 without a key, and a Delete or Rotate of this device would leave its key working as no key at all. Switch it on under Settings → Network & access."
                        </p>
                    </Show>
                    <Show
                        when=move || admin_on(&admin_tools.get())
                        fallback=|| view! {
                            <p class="dim mini-note">
                                "Never the admin plane: a device holds the Chat API and /v1, not this dashboard's settings."
                            </p>
                        }
                    >
                        <p class="notice warn">
                            {move || admin_level_warning(&admin_tools.get())}
                        </p>
                    </Show>
                </Show>
                <ModalFooter>
                    <button class="btn ghost" on:click=move |_| {
                        if reviewing.get_untracked() { reviewing.set(false) } else { open.set(false) }
                    }>
                        {move || if reviewing.get() { "Back" } else { "Cancel" }}
                    </button>
                    <Show
                        when=move || reviewing.get()
                        fallback=move || view! {
                            <button class="btn primary" on:click=review>"Review…"</button>
                        }
                    >
                        <button class="btn primary" disabled=move || busy.get() on:click=pair>
                            {move || if busy.get() { "Pairing…" } else { "Pair the device" }}
                        </button>
                    </Show>
                </ModalFooter>
            </div>
        </Modal>
    }
}

/// The pairing link just minted, shown once with Copy and Open (§1.4). Done
/// forgets it: lmgw keeps only the key's hash.
#[component]
fn LinkShown(open: RwSignal<bool>, minted: RwSignal<Option<Minted>>) -> impl IntoView {
    let toasts = use_toasts();
    // Gone from memory as the dialog closes, however it closes.
    Effect::new(move |_| {
        if !open.get() {
            minted.set(None);
        }
    });
    view! {
        <Modal open=open title="Pairing link">
            {move || {
                minted
                    .get()
                    .map(|m| {
                        let copy_link = m.link.clone();
                        let note = m.url_note.clone().map(|n| {
                            view! {
                                <p class="notice warn">
                                    <span class="mono-sm">{m.url.clone()}</span>
                                    " is " {n}
                                    " — a phone needs the address it reaches this gateway by; pair it again with that address."
                                </p>
                            }
                        });
                        view! {
                            <p>
                                {if m.rotated { "The new link for device " } else { "Device " }}
                                <b>{format!("'{}'", m.name)}</b>
                                {if m.rotated { "." } else { " is paired." }}
                                " The link carries its key and is shown only now: lmgw keeps the key's hash."
                            </p>
                            <div class="pair-link mono-sm">{m.link.clone()}</div>
                            <div class="row" style="margin-top:10px">
                                <a class="btn primary" href=m.link.clone()>
                                    "Open on this computer"
                                </a>
                                <button
                                    class="btn"
                                    on:click=move |_| copy_secret(
                                        &copy_link,
                                        toasts,
                                        "pairing link copied".into(),
                                    )
                                >
                                    "Copy link"
                                </button>
                            </div>
                            {note}
                            <p class="dim mini-note">
                                "Open starts the client app registered for lmgw-pair: links. To pair another machine, carry the link over a channel you trust; the client keeps the key in its secret store and drops the link."
                            </p>
                        }
                    })
            }}
            <ModalFooter>
                <button class="btn primary" on:click=move |_| open.set(false)>
                    "Done"
                </button>
            </ModalFooter>
        </Modal>
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
                },
                lmgw_api_types::OpenLinks {
                    kind: "voice".into(),
                    count: 1,
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
    fn a_loopback_address_is_named_as_one() {
        for u in [
            "http://127.0.0.1:8787",
            "http://localhost:8001",
            "http://[::1]:8001",
            "http://127.1.2.3",
        ] {
            assert!(loopback(u), "{u}");
        }
        for u in [
            "http://192.0.2.10:8787",
            "https://lmgw.example.net",
            "http://[fe80::1]:1",
            "http://127.example.net",
        ] {
            assert!(!loopback(u), "{u}");
        }
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
