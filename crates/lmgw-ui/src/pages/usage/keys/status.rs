//! Nothing selected: what the keys add up to and the ways to add one. The
//! global budget against this period's spend, the form that creates a key
//! (two kinds, one form; a client key can be created already fenced),
//! pairing a device with the link strip above it, the transfers between
//! devices, and why a budget refusal is a 403.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{KeyCreated, KeysResponse, ToolInventory};
use serde_json::json;

use super::super::devices::{LinkStrip, TransfersSection};
use super::super::scope_editor::{AliasScopeEditor, ScopeState, ToolScopeEditor};
use super::super::*;
use super::detail::FreshNote;
use super::{Fresh, KeysCtx};
use crate::fmt::grouped;
use crate::widgets::nav_split::{track_scroll, DetailHead};
use crate::widgets::{
    use_toasts, ActionRow, ComboRow, EntryRow, Explain, Ico, Icon, PrefGroup, StackRow, SwitchRow,
};

#[component]
pub(super) fn StatusPage(page: KeysCtx, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    let keys = page.keys;
    let devices = Memo::new(move |_| {
        keys.data.with(|d| {
            d.as_ref()
                .map(|k| k.keys.iter().filter(|r| r.kind == "device").count())
        })
    });
    let auth_off = move || {
        keys.data
            .with(|d| d.as_ref().is_some_and(|k| !k.auth_enabled))
    };
    view! {
        <DetailHead narrow=narrow back=back title="Keys & budgets" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow kstatus">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Key/>
                    </div>
                    <div class="hero-text">
                        <h1>"Keys & budgets"</h1>
                        <div class="sub">
                            "Every credential and internal identity, with its scope, budget and spend. "
                            "Pick one in the list, or add one."
                        </div>
                    </div>
                </div>
                {move || page.fresh.get().map(|f| view! { <FreshNote page=page fresh=f/> })}
                <PrefGroup title="Global budget">
                    <GlobalBudget keys=keys/>
                    <a
                        class="prow action"
                        href=crate::pages::settings_href("global_budget_micro")
                        title="The global budget and its period are gateway settings"
                    >
                        <div class="prow-text">
                            <span class="prow-title">"Set the global budget"</span>
                            <span class="prow-sub">"Settings → Usage & cost"</span>
                        </div>
                        <Icon ico=Ico::Right class="prow-chevron"/>
                    </a>
                </PrefGroup>
                <KeyCreate page=page/>
                <PrefGroup
                    title="Devices"
                    desc="A client app — a desktop assistant, a phone — pairs with a link that carries a key of its own: the Chat API and /v1 under that key's scope and budget, and nothing of this dashboard."
                >
                    <Show when=auth_off>
                        <div class="prow notice-row warn">
                            "Require API key is off: anyone who reaches this gateway's address uses /v1 "
                            "without a key, whatever a device's scope and budget say. Switch it on under "
                            <a href="/settings">"Settings → Network & access"</a>
                            "."
                        </div>
                    </Show>
                    <ActionRow
                        title="Pair a device…"
                        sub=Signal::derive(move || match devices.get() {
                            None => String::new(),
                            Some(0) => "No device is paired yet".to_string(),
                            Some(n) => format!("{} paired: under Devices in the list", crate::fmt::count_of(n, "devices")),
                        })
                        chevron=true
                        on_click=Callback::new(move |()| page.devices.pair_open.set(true))
                    />
                    <StackRow title="Device link" class="dev-strip-row">
                        <LinkStrip keys=keys refresh=page.refresh/>
                    </StackRow>
                </PrefGroup>
                <PrefGroup title="Transfers">
                    <StackRow class="xfer-row">
                        <TransfersSection/>
                    </StackRow>
                </PrefGroup>
                <div class="pref-group kexplain">
                    <Explain
                        summary="A budget refusal answers 403 key_budget, deliberately not a 429."
                        persist="usage.explain.keys"
                    >
                        "A monthly budget will not clear inside any retry window, so a client that "
                        "retried a 429 would only hammer it. A rate limit keeps its 429 and a real "
                        <span class="mono-sm">"Retry-After"</span>
                        ". Cost is only known after a response, so one request always crosses a budget "
                        "line — the overshoot is shown rather than clamped. A browser signed in with an "
                        "owner key stays logged in for a year, or until Rotate. A client key is what a "
                        "caller presents on " <code>"/v1"</code> "; an owner key holds every capability, "
                        "signs a browser in, and is the credential " <code>"/api"</code> " and "
                        <code>"/mcp/admin"</code> " ask for — it is named " <code>"owner:<name>"</code>
                        " and stays copyable from its page. A device key is a paired client app's: the "
                        "Chat API and " <code>"/v1"</code> " under its own scope and budget, hash-only — "
                        "New token pairs it again."
                    </Explain>
                </div>
            </div>
        </div>
    }
}

/// The owner's ceiling across every key and this period's spend against it —
/// the figure that answers "am I over" (the Budget chart draws the window's
/// cumulative line against the same ceiling).
#[component]
fn GlobalBudget(keys: Src<KeysResponse>) -> impl IntoView {
    let line = move || {
        let k = keys.data.get()?;
        let over = k.global_budget_micro > 0 && k.global_spent_micro > k.global_budget_micro;
        let (title, sub) = if k.global_budget_micro > 0 {
            let mut t = format!(
                "{} of {}",
                money(k.global_spent_micro, &k.currency),
                money(k.global_budget_micro, &k.currency),
            );
            if over {
                t.push_str(&format!(
                    " — over by {}",
                    money(k.global_spent_micro - k.global_budget_micro, &k.currency)
                ));
            }
            let mut s = format!("this {}", period_word(&k.global_budget_period));
            if k.global_spent_unknown_requests > 0 {
                s.push_str(&format!(
                    " · {} requests against it unpriced",
                    grouped(k.global_spent_unknown_requests as u64)
                ));
            }
            (t, s)
        } else {
            (
                "None set".to_string(),
                "Every key spends without a gateway-wide ceiling".to_string(),
            )
        };
        let ratio = if k.global_budget_micro > 0 {
            (k.global_spent_micro as f64 / k.global_budget_micro as f64).min(1.0)
        } else {
            0.0
        };
        Some(view! {
            <div class="prow keys-budget" class:over=over>
                <div class="prow-text">
                    <span class="prow-title">{title}</span>
                    <span class="prow-sub">{sub}</span>
                </div>
                {(k.global_budget_micro > 0)
                    .then(|| {
                        view! {
                            <div class="meter" class:over=over>
                                <i style=format!("width:{:.1}%", ratio * 100.0)></i>
                            </div>
                        }
                    })}
            </div>
        })
    };
    view! {
        {line}
        <Show when=move || keys.data.with(Option::is_none)>
            <div class="prow">
                <span class="prow-sub">
                    {move || if keys.err.with(Option::is_some) { "Not loaded" } else { "Loading…" }}
                </span>
            </div>
        </Show>
    }
}

/// Mint a key. Two kinds, one form: the only difference is what the key may
/// do, and the server prefixes an owner key's name with `owner:` itself
/// (principals §3.12). It acts at once — a key is not a draft — and opens
/// the new key, whose page shows its plaintext once.
#[component]
fn KeyCreate(page: KeysCtx) -> impl IntoView {
    let toasts = use_toasts();
    let name = RwSignal::new(String::new());
    let kind = RwSignal::new("client".to_string());
    let busy = RwSignal::new(false);
    // The Scope rows: a client key can be created already fenced, so it is
    // never wide open between the create and a first edit.
    let scope = ScopeState::new();
    let alive = crate::scope::Scope::new();
    let scope_open = RwSignal::new(false);
    let inventory = LocalResource::new(move || {
        let wanted = scope_open.get();
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
    let kind_opts = Signal::derive(|| {
        vec![
            ("client".to_string(), "Client key".to_string()),
            ("owner".to_string(), "Owner key".to_string()),
        ]
    });
    let box_ref: NodeRef<leptos::html::Section> = NodeRef::new();
    // + → New key… puts the focus into the name.
    Effect::new(move |prev: Option<u32>| {
        let n = page.create_focus.get();
        if prev.is_some_and(|p| p != n) || (prev.is_none() && n > 0) {
            request_animation_frame(move || {
                if let Some(el) = box_ref.get_untracked() {
                    if let Ok(Some(i)) = el.query_selector("input") {
                        use wasm_bindgen::JsCast;
                        if let Ok(i) = i.dyn_into::<web_sys::HtmlElement>() {
                            let _ = i.focus();
                        }
                    }
                }
            });
        }
        n
    });
    let create = move || {
        let n = name.get_untracked().trim().to_string();
        if n.is_empty() || busy.get_untracked() {
            return;
        }
        let k = kind.get_untracked();
        let mut body = json!({ "name": n, "kind": k });
        // Only a client key has a scope, and only a scope that narrows
        // anything is sent.
        if k == "client" && scope_open.get_untracked() && !scope.is_default() {
            if scope.tool_mode.get_untracked() != "all" && scope.groups.get_untracked().is_none() {
                toasts.err("the tool list is still loading — try again in a moment");
                return;
            }
            body["scope_mode"] = json!(scope.scope_mode.get_untracked());
            body["scope_patterns"] = json!(untrack(|| scope.alias_patterns()));
            body["tool_scope_mode"] = json!(scope.tool_mode.get_untracked());
            body["tool_scope_patterns"] = json!(untrack(|| scope.tool_patterns()));
        }
        busy.set(true);
        let key_name = n.clone();
        spawn_local(async move {
            let res = crate::api::post::<KeyCreated, _>("/api/op/key_create", &body).await;
            let v = match res {
                Ok(v) => v,
                Err(e) => {
                    busy.try_set(false);
                    toasts.err(e.to_string());
                    return;
                }
            };
            // The plaintext goes to the page, not to this form: a row picked
            // in the list while the POST was out ends this status page, and
            // for a client key this answer is the only time the key is ever
            // legible. Its own page shows it, until dismissed.
            let id = v.id;
            let named = v.name.clone();
            let kept = page
                .fresh
                .try_set(Some(Fresh {
                    id,
                    name: v.name,
                    key: v.key,
                }))
                .is_none();
            page.refresh.try_update(|n| *n = n.wrapping_add(1));
            if !alive.alive() {
                // Leaving the status page disposed the form's signals;
                // reading one (`scope.load`) would trap the module.
                toasts.warn(if !kept {
                    if k == "owner" {
                        format!(
                            "key '{named}' was created, but Keys & budgets was left before it \
                             could be shown; it stays copyable from its page there"
                        )
                    } else {
                        format!(
                            "key '{key_name}' was created, but Keys & budgets was left before \
                             its secret could be shown, and it is not shown again: revoke it on \
                             its page there and create another"
                        )
                    }
                } else {
                    format!("key '{named}' was created: its page in the list shows it")
                });
                return;
            }
            busy.set(false);
            name.set(String::new());
            scope.load("all", "", "all", "");
            scope_open.set(false);
            let q = super::list_query(&window().location().search().unwrap_or_default());
            page.go(&format!("/usage/keys/{id}{q}"), false);
        });
    };
    let blank = move || name.with(|n| n.trim().is_empty());
    let client = move || kind.get() == "client";
    view! {
        <section class="pref-group key-create" node_ref=box_ref>
            <div class="pref-head">
                <h2>"New key"</h2>
                <span class="pref-suffix">
                    <span class="applies-now">"applies now"</span>
                </span>
            </div>
            <p class="pref-desc">
                "A client key is what a caller presents on /v1, under its scope and budget; its "
                "plaintext is shown once. An owner key holds every capability and stays copyable."
            </p>
            <div class="boxed">
                <div on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Enter" {
                        create();
                    }
                }>
                    <EntryRow title="Name" value=name placeholder="new key name"/>
                </div>
                <ComboRow title="Kind" value=kind options=kind_opts/>
                <Show when=client>
                    <SwitchRow
                        title="Fence it now"
                        sub="Narrow its aliases and tools before it exists, so it is never wide open"
                        value=scope_open
                    />
                </Show>
                <Show when=move || scope_open.get() && client()>
                    <div class="key-create-scope">
                        <AliasScopeEditor state=scope disabled=Signal::derive(|| false)/>
                        <ToolScopeEditor
                            state=scope
                            inventory=inv_state
                            disabled=Signal::derive(|| false)
                        />
                    </div>
                </Show>
                <div class="prow key-create-go">
                    <div class="prow-text">
                        <span class="prow-sub">
                            {move || {
                                if client() {
                                    "Creates the key at once and shows its plaintext on its page, once"
                                } else {
                                    "Creates the key at once, named owner:<name>"
                                }
                            }}
                        </span>
                    </div>
                    <div class="prow-end">
                        <button
                            class="btn suggested"
                            disabled=move || blank() || busy.get()
                            on:click=move |_| create()
                        >
                            {move || if busy.get() { "Creating…" } else { "Create key" }}
                        </button>
                    </div>
                </div>
            </div>
        </section>
    }
}
