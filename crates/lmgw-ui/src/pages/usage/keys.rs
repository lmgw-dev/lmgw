use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{KeyRow, KeysResponse, PricesResponse, ToolInventory};
use serde_json::{json, Value};

use crate::fmt::grouped;
use crate::live::use_live;
use crate::widgets::{use_toasts, CopyBtn, Explain, Modal, PageFrame, PageMode, Select};

use super::scope_editor::{AliasScopeEditor, ScopeState, ToolScopeEditor};
use super::*;

/// Keys & budgets (`/usage/keys`): every credential and internal identity
/// with its policy and spend — and the one place a key is created or revoked
/// (Settings → Network & access links here).
#[component]
pub fn UsageKeys() -> impl IntoView {
    let live = use_live();
    let refresh = RwSignal::new(0u32);
    let keys = src(LocalResource::new(move || {
        refresh.track();
        crate::api::get::<KeysResponse>("/api/usage/keys")
    }));
    // The Prices tab's count, once.
    let prices = src(LocalResource::new(|| {
        crate::api::get::<PricesResponse>("/api/usage/prices")
    }));
    // The plaintext a create just returned. It lives out here, drawn in the
    // page's top strip, not in the list: the list refetches (after the create
    // itself, and as spend lands), and for a client key this line is the only
    // time it is ever legible (principals §3.12).
    let fresh = RwSignal::new(None::<(String, String)>);

    // Nothing here polls. Spend and last-used move as requests land: one
    // refetch at most 15 s after the first that landed since the last one.
    // A device's online state and a revocation move without a request: the
    // `keys` frame of `/api/events` says so, and the list is refetched at
    // once (client-apps design §1.6).
    let pending = RwSignal::new(None::<TimeoutHandle>);
    let refetch = move || refresh.update(|v| *v = v.wrapping_add(1));
    Effect::new(move |_| {
        if live.request.get().is_none() || pending.get_untracked().is_some() {
            return;
        }
        let later = set_timeout_with_handle(
            move || {
                pending.set(None);
                refetch();
            },
            Duration::from_secs(15),
        );
        pending.set(later.ok());
    });
    Effect::new(move |_| {
        if live.keys.get().is_some() {
            refetch();
        }
    });
    on_cleanup(move || {
        if let Some(h) = pending.get_untracked() {
            h.clear();
        }
    });

    view! {
        <PageFrame
            title="Usage"
            sub="spend against budget · scope · rate limit"
            mode=PageMode::Fill
            head_extra=move || view! { <UsageTabs keys=keys prices=prices/> }
            toolbar=move || view! { <FreshKey fresh=fresh/> }
        >
            <div class="keys-top">
                <GlobalBudget keys=keys/>
                <KeyCreate fresh=fresh refresh=refresh/>
            </div>
            <KeysTable keys=keys refresh=refresh/>
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
                " and stays copyable from its row. A device key is a paired client app's: the "
                "Chat API and " <code>"/v1"</code> " under its own scope and budget, hash-only — "
                "Rotate pairs it again."
            </Explain>
        </PageFrame>
    }
}

/// The key a create just returned, with a copy button, until dismissed.
#[component]
fn FreshKey(fresh: RwSignal<Option<(String, String)>>) -> impl IntoView {
    move || {
        fresh.get().map(|(n, key)| {
            // An owner key stays copyable from its row, where Rotate is; a
            // client key is this line and then never again (principals
            // §3.12).
            let kept = n.starts_with("owner:");
            view! {
                <div class="wiz-ok row">
                    <span>
                        "Key '" {n} "' created"
                        {if kept {
                            " — it stays copyable from its row:"
                        } else {
                            " — copy it now, it is not shown again:"
                        }}
                    </span>
                    <span class="mono-sm">{key.clone()}</span>
                    <CopyBtn text=key title="Copy the key"/>
                    <span class="spacer" style="flex:1"></span>
                    <button class="btn ghost sm" on:click=move |_| fresh.set(None)>
                        "Dismiss"
                    </button>
                </div>
            }
        })
    }
}

/// The owner's ceiling across every key and this period's spend against it —
/// the figure that answers "am I over" (the Budget chart draws the window's
/// cumulative line against the same ceiling).
#[component]
fn GlobalBudget(keys: Src<KeysResponse>) -> impl IntoView {
    let line = move || {
        let k = keys.data.get()?;
        let text = if k.global_budget_micro > 0 {
            let over = k.global_spent_micro - k.global_budget_micro;
            let mut t = format!(
                "{} of {} this {}",
                money(k.global_spent_micro, &k.currency),
                money(k.global_budget_micro, &k.currency),
                period_word(&k.global_budget_period),
            );
            if over > 0 {
                t.push_str(&format!(" — over by {}", money(over, &k.currency)));
            }
            if k.global_spent_unknown_requests > 0 {
                t.push_str(&format!(
                    " · {} requests against it unpriced",
                    grouped(k.global_spent_unknown_requests as u64)
                ));
            }
            t
        } else {
            "none set — every key spends without a gateway-wide ceiling".to_string()
        };
        let over = k.global_budget_micro > 0 && k.global_spent_micro > k.global_budget_micro;
        Some(view! {
            <span class="keys-budget" class:over=over>
                <span class="lbl">"Global budget"</span>
                {text}
            </span>
        })
    };
    view! {
        {line}
        <a
            class="dim"
            href=crate::pages::settings_href("global_budget_micro")
            title="The global budget and its period are gateway settings"
        >
            "Settings → Usage & cost"
        </a>
    }
}

/// Mint a key. Two kinds, one form: the only difference is what the key may
/// do, and the server prefixes an owner key's name with `owner:` itself
/// (principals §3.12). It acts at once — a key is not a draft.
#[component]
fn KeyCreate(fresh: RwSignal<Option<(String, String)>>, refresh: RwSignal<u32>) -> impl IntoView {
    let toasts = use_toasts();
    let name = RwSignal::new(String::new());
    let kind = RwSignal::new("client".to_string());
    let busy = RwSignal::new(false);
    // The collapsed Scope section: a client key can be created already
    // fenced, so it is never wide open between the create and a first edit.
    let scope = ScopeState::new();
    let page = crate::scope::Scope::new();
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
            ("client".to_string(), "client key".to_string()),
            ("owner".to_string(), "owner key".to_string()),
        ]
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
        if k == "client" && !scope.is_default() {
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
            let res = crate::api::post::<Value, _>("/api/op/key_create", &body).await;
            // Leaving the page while the POST was out disposed the signals
            // below; reading one (`scope.load`) would trap the module.
            if !page.alive() {
                match res {
                    Ok(_) => toasts.warn(format!(
                        "key '{key_name}' was created, but this page was left before its secret \
                         could be shown — rotate it"
                    )),
                    Err(e) => toasts.err(e.to_string()),
                }
                return;
            }
            busy.set(false);
            match res {
                Ok(v) => {
                    let text = |f: &str| {
                        v.get(f)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    fresh.set(Some((text("name"), text("plaintext"))));
                    name.set(String::new());
                    scope.load("all", "", "all", "");
                    scope_open.set(false);
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let blank = move || name.with(|n| n.trim().is_empty());
    view! {
        <div class="key-create">
            <input
                class="input w-sm"
                placeholder="new key name"
                aria-label="New key name"
                prop:value=move || name.get()
                on:input=move |ev| name.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() == "Enter" {
                        create();
                    }
                }
            />
            <Select value=kind options=kind_opts/>
            <button
                class="btn"
                disabled=move || blank() || busy.get()
                title="Creates the key at once and shows its plaintext above"
                on:click=move |_| create()
            >
                {move || if busy.get() { "Creating…" } else { "Create key" }}
            </button>
            <Show when=move || kind.get() == "client">
                <button
                    class="btn ghost"
                    aria-expanded=move || scope_open.get().to_string()
                    on:click=move |_| scope_open.update(|o| *o = !*o)
                >
                    {move || if scope_open.get() { "Scope ▾" } else { "Scope ▸" }}
                </button>
            </Show>
            <span class="applies-now">"applies now"</span>
            <Show when=move || scope_open.get() && kind.get() == "client">
                <div class="key-create-scope">
                    <AliasScopeEditor state=scope disabled=Signal::derive(|| false)/>
                    <ToolScopeEditor
                        state=scope
                        inventory=inv_state
                        disabled=Signal::derive(|| false)
                    />
                </div>
            </Show>
        </div>
    }
}

/// Every key as one table row: policy, spend, and the row's actions. Revoke
/// is only offered where it is the owner's call — see [`revocable`].
#[component]
fn KeysTable(keys: Src<KeysResponse>, refresh: RwSignal<u32>) -> impl IntoView {
    let toasts = use_toasts();
    let editing = RwSignal::new(None::<KeyRow>);
    let open = RwSignal::new(false);
    // The owner row waiting on the rotate confirm. Rotation has no grace
    // window and the dashboard's own key takes every browser with it
    // (principals §3.12), so it is never one click.
    let rotating = RwSignal::new(None::<KeyRow>);
    let rotate_open = RwSignal::new(false);
    // The row name and the plaintext a rotate just returned. That response is
    // the only place the key is legible without a session, and rotating
    // `owner:dashboard` is quite capable of taking this tab's — so it goes on
    // screen for every owner row, before anything is attempted with it.
    let rotated = RwSignal::new(None::<(String, String)>);
    let rotated_open = RwSignal::new(false);
    // The re-login after a dashboard rotate did not land: this tab is holding a
    // cookie minted from a key that no longer exists.
    let stranded = RwSignal::new(false);
    let rotate = move |_| {
        let Some(k) = rotating.get_untracked() else {
            return;
        };
        rotate_open.set(false);
        spawn_local(async move {
            let res =
                crate::api::post::<Value, _>("/api/op/key_rotate", &json!({ "id": k.id })).await;
            match res {
                Ok(v) => {
                    let key = v
                        .get("key")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let msg = v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("key rotated")
                        .to_string();
                    stranded.set(false);
                    if !key.is_empty() {
                        rotated.set(Some((k.name.clone(), key.clone())));
                        rotated_open.set(true);
                    }
                    // The cookie this tab is holding was minted from the key
                    // that just stopped existing. Present the new one now, or
                    // the very next call on this page lands on the login card.
                    if k.name == "owner:dashboard" && !key.is_empty() {
                        if let Err(e) = crate::session::sign_in(&key).await {
                            stranded.set(true);
                            // Two ways back, because the modal is only as
                            // durable as the page under it: the key is on
                            // screen, and the login card is holding it too.
                            crate::session::carry_key(&key);
                            toasts.err(format!("key rotated, but this tab did not re-log-in: {e}"));
                        }
                    }
                    toasts.ok(msg);
                    // A refetch on a dead cookie answers 401, which raises the
                    // gate and takes this modal — and the readable copy of the
                    // key — down with it. Signing back in re-runs the page
                    // anyway. (And a page that is gone has nothing to refetch.)
                    if stranded.try_get_untracked() == Some(false) {
                        refresh.update(|v| *v = v.wrapping_add(1));
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let revoke = move |id: i64, name: String| {
        spawn_local(async move {
            match crate::api::post::<Value, _>("/api/op/key_delete", &json!({ "id": id })).await {
                Ok(_) => {
                    toasts.ok(format!("{name} revoked"));
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    // Keyed by what only an edit changes: spend, requests and last used move
    // with every landed request (a refetch every 15 s under traffic), and a
    // rebuilt row would drop an armed "Revoke …? ✓", close an open ⋯ menu
    // and lose focus (review code:U4). Those cells follow the data in place.
    let idents = Memo::new(move |_| {
        keys.data.with(|d| {
            d.as_ref().map(|k| {
                // A device is listed on its own card, with the actions that
                // say what they end (client-apps design §1.4).
                k.keys
                    .iter()
                    .filter(|r| r.kind != "device")
                    .map(|r| KeyIdent {
                        id: r.id,
                        name: r.name.clone(),
                        kind: r.kind.clone(),
                        enabled: r.enabled,
                        revocable: revocable(r),
                    })
                    .collect::<Vec<_>>()
            })
        })
    });
    let revoke = Callback::new(move |(id, name): (i64, String)| revoke(id, name));
    let edit = Callback::new(move |r: KeyRow| {
        editing.set(Some(r));
        open.set(true);
    });
    let rotate_ask = Callback::new(move |r: KeyRow| {
        rotating.set(Some(r));
        rotate_open.set(true);
    });
    let placeholder = move || {
        match idents.get() {
        None => Some(
            view! {
                <tr>
                    <td colspan=KEY_COLS class="dim">
                        {move || if keys.err.with(Option::is_some) { "" } else { "Loading…" }}
                    </td>
                </tr>
            }
            .into_any(),
        ),
        Some(v) if v.is_empty() => Some(
            view! {
                <tr>
                    <td colspan=KEY_COLS class="wrap dim">
                        "No keys yet. Create one above; internal consumers (admin chat, quickdoc ingest, …) appear here as identities once they spend."
                    </td>
                </tr>
            }
            .into_any(),
        ),
        Some(_) => None,
    }
    };
    let rows = view! {
        {placeholder}
        <For each=move || idents.get().unwrap_or_default() key=|k| k.clone() let:k>
            <KeyLine ident=k keys=keys revoke=revoke edit=edit rotate=rotate_ask/>
        </For>
    };

    view! {
        <CardErr err=keys.err/>
        // The paired devices first — a handful, each with its own actions —
        // as a strip above the table, which stays the page's one scroller
        // (and so keeps its sticky head).
        <DevicesCard keys=keys refresh=refresh edit=edit/>
        <div class="fill-pane hug card pad0" class:stale=move || keys.stale()>
            // Not `many-cols` (its p2 fold at 1180 hid rpm/tpm at 1440, where
            // main showed them): they fold under 1000, where Scope and Spend
            // would otherwise clip to a word. The name's tooltip has them.
            <table class="data keys-table">
                <thead>
                    <tr>
                        <th>"Key"</th>
                        <th>"Scope"</th>
                        <th>"Spend vs budget"</th>
                        <th class="num-h col-p2">"rpm"</th>
                        <th class="num-h col-p2">"tpm"</th>
                        <th
                            class="num-h"
                            title="requests in each key's own budget period: this month, today, or all time"
                        >
                            "Requests · period"
                        </th>
                        <th class="num-h col-p3" title="the hour of the key's last request, from the rollup">
                            "Last used"
                        </th>
                        <th></th>
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
        </div>
        <KeyEditor open=open editing=editing refresh=refresh/>

        // What rotating this particular key costs, before the click: the
        // dashboard key is every browser's session, the others are
        // whatever config presents them.
        <Modal open=rotate_open title="Rotate this owner key">
            <p class="dim">
                "The key is replaced in one write — there is no grace window. Anything "
                "holding the old one is locked out at once."
            </p>
            {move || {
                let dashboard = rotating.get().is_some_and(|k| k.name == "owner:dashboard");
                if dashboard {
                    view! {
                        <p class="notice warn">
                            "This is the key every browser's session is minted from. Every "
                            "other open tab lands on the login card; this one is signed back "
                            "in with the new key and carries on. The key is shown to you "
                            "either way, before that is attempted."
                        </p>
                    }
                    .into_any()
                } else {
                    view! {
                        <p class="notice warn">
                            "Whatever presents this key — an MCP client config, a curl in a "
                            "note — keeps sending the old one until you update it. The new "
                            "key is shown as soon as it exists, and stays copyable from this "
                            "row."
                        </p>
                    }
                    .into_any()
                }
            }}
            <div class="row" style="justify-content:flex-end; margin-top:12px">
                <button class="btn ghost" on:click=move |_| rotate_open.set(false)>
                    "Cancel"
                </button>
                <button class="btn danger" on:click=rotate>
                    "Rotate the key"
                </button>
            </div>
        </Modal>

        // The result: the plaintext itself, shown before it is used for
        // anything. `key_reveal` can read it back later — but only for a
        // browser that still has a session, which is exactly what rotating
        // the dashboard key takes away.
        <Modal open=rotated_open title="The new owner key">
            {move || {
                let Some((name, key)) = rotated.get() else { return ().into_any() };
                let copy_text = key.clone();
                let carried = key.clone();
                let stranded_note = move || {
                    if !stranded.get() {
                        return ().into_any();
                    }
                    let k = carried.clone();
                    view! {
                        <div class="notice err" style="margin-top:10px">
                            "sign back in with this key: the tab's session was rotated away"
                        </div>
                        <div class="row" style="justify-content:flex-end; margin-top:8px">
                            <button
                                class="btn primary"
                                title="opens the login card with this key already in the box"
                                on:click=move |_| crate::session::lock_with_key(&k)
                            >
                                "Open the login card"
                            </button>
                        </div>
                    }
                    .into_any()
                };
                view! {
                    <p class="dim">
                        "The new plaintext for "
                        <span class="mono-sm">{name}</span>
                        ". It is stored, so "
                        <b>"Copy"</b>
                        " on its row gets it back — for as long as this browser has a session."
                    </p>
                    <div class="wiz-ok" style="margin-top:10px">
                        <span class="mono-sm">{key}</span>
                        " "
                        <CopyBtn text=copy_text title="Copy the new key"/>
                    </div>
                    {stranded_note}
                }
                .into_any()
            }}
            <div class="row" style="justify-content:flex-end; margin-top:12px">
                <button class="btn primary" on:click=move |_| rotated_open.set(false)>
                    "Done"
                </button>
            </div>
        </Modal>
    }
}
