//! "Pair a device" (client-apps design §1.4, device link design §3.2, §4.1):
//! a name, where the device is (this computer, the LAN, WireGuard), the URL
//! list lmgw proposes for it and the owner edits, the scopes and the budget
//! — shown, prefilled `all` and no budget (§11 Q1) —, the other devices'
//! tools it may use (the device-tools grant, none unless ticked) and an
//! optional hosting label. The review step restates what the device will
//! reach, and what the device listener must turn on or widen first, with
//! the exposure said (D24); the button does it after that confirmation, and
//! then the key is minted and the QR card opens.

use leptos::task::spawn_local;
use lmgw_api_types::{
    ConnectInfo, DevicePaired, KeysResponse, MessageAck, SettingsFull, ToolInventory,
};
use serde_json::json;

use super::device_tools::{
    grant_words, hosted_labels, unhosted, DeviceToolsEditor, DeviceToolsState,
};
use super::logic::{self, Need, Where};
use super::qr_card::QrSession;
use super::scope_editor::{AliasScopeEditor, ScopeState, ToolScopeEditor};
use super::*;
use crate::widgets::{use_toasts, Modal, ModalFooter, Select};

#[component]
pub(super) fn PairDevice(
    open: RwSignal<bool>,
    keys: Src<KeysResponse>,
    refresh: RwSignal<u32>,
    session: RwSignal<Option<QrSession>>,
    card_open: RwSignal<bool>,
) -> impl IntoView {
    let toasts = use_toasts();
    let reviewing = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let where_ = RwSignal::new(Where::Computer.key().to_string());
    let urls_text = RwSignal::new(String::new());
    let label = RwSignal::new(String::new());
    // The device's level of lmgw's admin tools: off unless chosen.
    let admin_tools = RwSignal::new("off".to_string());
    let budget = RwSignal::new(String::new());
    let period = RwSignal::new("month".to_string());
    let busy = RwSignal::new(false);
    let scope = ScopeState::new();
    // The paired devices' tools it may use — Kai desktop's, say (client-apps
    // design L16 as changed 2026-10-15): none unless ticked.
    let device_tools = DeviceToolsState::new();
    let hosted = Signal::derive(move || {
        keys.data.with(|d| {
            d.as_ref()
                .map(|k| hosted_labels(&k.keys))
                .unwrap_or_default()
        })
    });
    let page = crate::scope::Scope::new();

    // The proposal's source: the device port and the addresses a device can
    // count on (`GET /api/connect`), read each time the form opens.
    let connect = LocalResource::new(move || {
        let wanted = open.get();
        async move {
            if !wanted {
                return None;
            }
            crate::api::get::<ConnectInfo>("/api/connect").await.ok()
        }
    });
    // The device listener's settings, for what pairing must turn on or widen.
    let settings = LocalResource::new(move || {
        let wanted = open.get();
        async move {
            if !wanted {
                return None;
            }
            crate::api::get::<SettingsFull>("/api/settings-full")
                .await
                .ok()
                .map(|s| s.devices)
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

    let w = Memo::new(move |_| Where::from_key(&where_.get()));
    let proposed = Memo::new(move |_| {
        connect
            .get()
            .flatten()
            .map(|c| (c.device_port, c.device_urls))
            .unwrap_or_default()
    });
    // The list follows "where" and the proposal; the owner edits it after.
    Effect::new(move |_| {
        let (port, urls) = proposed.get();
        urls_text.set(logic::proposal(w.get(), port, &urls).join("\n"));
    });
    let urls = Memo::new(move |_| logic::parse_urls(&urls_text.get()));
    // What lmgw knows about each proposed address still in the list.
    let notes = Memo::new(move |_| {
        let listed = urls_text.get();
        let listed: Vec<&str> = listed.lines().map(str::trim).collect();
        proposed
            .get()
            .1
            .iter()
            .filter(|u| listed.contains(&u.url.as_str()))
            .map(|u| (u.url.clone(), logic::url_note(u)))
            .collect::<Vec<_>>()
    });
    let none_found = Memo::new(move |_| {
        let (port, urls) = proposed.get();
        connect.get().flatten().is_some() && logic::proposal(w.get(), port, &urls).is_empty()
    });
    let dev_settings = Memo::new(move |_| settings.get().flatten());
    let need = Memo::new(move |_| {
        dev_settings
            .get()
            .map(|s| logic::listener_need(&s, w.get().local_only()))
    });

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
    // The grant as the review says it, a label no device hosts named so.
    let device_tools_words = move || {
        let grant = device_tools.labels();
        let waiting = unhosted(&grant, &hosted.get());
        if waiting.is_empty() {
            grant_words(&grant)
        } else {
            format!(
                "{} (no paired device hosts {} now)",
                grant_words(&grant),
                waiting.join(", ")
            )
        }
    };
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
    // Require API key off and a device beyond this computer (review W2-9):
    // anyone who reaches this address is served without a key, and a Delete
    // or New token of a stolen device leaves its key working as no key at all.
    let open_door = move || {
        keys.data
            .with(|d| d.as_ref().is_some_and(|k| !k.auth_enabled))
            && !w.get().local_only()
    };

    let reset = move || {
        reviewing.set(false);
        name.set(String::new());
        label.set(String::new());
        admin_tools.set("off".into());
        budget.set(String::new());
        period.set("month".into());
        where_.set(Where::Computer.key().to_string());
        scope.load("all", "", "all", "");
        device_tools.load(&[], &[]);
    };
    let review = move |_| {
        if name.get_untracked().trim().is_empty() {
            toasts
                .err("name the device first — it is how its card and its voice takeovers name it");
            return;
        }
        let list = match urls.get_untracked() {
            Ok(l) => l,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        if w.get_untracked().local_only() {
            if let Some(far) = list.iter().find(|u| !logic::url_is_loopback(u)) {
                toasts.err(format!(
                    "'{far}' reaches beyond this computer: choose the LAN or WireGuard, or keep \
                     only loopback addresses"
                ));
                return;
            }
        }
        if budget_micro.get_untracked().is_none() {
            toasts.err("budget must be an amount like 10 or 10.50 — empty means none");
            return;
        }
        if scope.tool_mode.get_untracked() != "all" && scope.groups.get_untracked().is_none() {
            toasts.err("the tool list is still loading — try again in a moment");
            return;
        }
        if dev_settings.get_untracked().is_none() {
            toasts.err("the device listener's settings are still loading — try again in a moment");
            return;
        }
        reviewing.set(true);
    };
    // `apply`: do what the review says the listener needs (turn on, widen)
    // first. Without it the pairing is written as it is, which a listener
    // that is off refuses by name.
    let pair = move |apply: bool| {
        if busy.get_untracked() {
            return;
        }
        let (Some(s), Ok(list)) = (dev_settings.get_untracked(), urls.get_untracked()) else {
            return;
        };
        let need = logic::listener_need(&s, w.get_untracked().local_only());
        let patch = if apply && need != Need::Nothing {
            match logic::need_patch(&s, &need) {
                Ok(p) => Some(p),
                Err(e) => {
                    toasts.err(e);
                    return;
                }
            }
        } else {
            None
        };
        let mut body = json!({
            "kind": "device",
            "name": name.get_untracked().trim(),
            "local_only": w.get_untracked().local_only(),
            "urls": list,
            "scope_mode": scope.scope_mode.get_untracked(),
            "scope_patterns": untrack(|| scope.alias_patterns()),
            "tool_scope_mode": scope.tool_mode.get_untracked(),
            "tool_scope_patterns": untrack(|| scope.tool_patterns()),
            "device_tools": untrack(|| device_tools.labels()),
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
            if let Some(patch) = patch {
                let res =
                    crate::api::post::<MessageAck, _>("/api/op/settings_set_full", &patch).await;
                match res {
                    Ok(a) => toasts.ok(a.message),
                    Err(e) => {
                        busy.set(false);
                        toasts.err(e.to_string());
                        return;
                    }
                }
            }
            let res = crate::api::post::<DevicePaired, _>("/api/op/key_create", &body).await;
            if !page.alive() {
                if res.is_ok() {
                    toasts.warn(
                        "the device was paired, but this page was left before its link could be \
                         shown — give it a new token on the Devices card",
                    );
                }
                return;
            }
            busy.set(false);
            match res {
                Ok(v) => {
                    session.set(Some(QrSession::from_answer(v, false)));
                    open.set(false);
                    card_open.set(true);
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
    let where_opts = Signal::derive(|| {
        Where::ALL
            .into_iter()
            .map(|w| (w.key().to_string(), w.label().to_string()))
            .collect::<Vec<_>>()
    });
    let need_lines = move || {
        let (Some(s), Some(n)) = (dev_settings.get(), need.get()) else {
            return Vec::new();
        };
        logic::need_text(&s, &n)
    };
    let listing = move || {
        urls.get()
            .map(|l| l.join("\n"))
            .unwrap_or_else(|_| urls_text.get())
    };

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
                                <span class="lbl">"where"</span>
                                <Select value=where_ options=where_opts/>
                                <span class="dim mini-note">
                                    {move || match w.get() {
                                        Where::Computer => "a client app on this computer: reachable from this computer only",
                                        Where::Lan => "another device on your network",
                                        Where::Wireguard => "another device over a WireGuard tunnel",
                                    }}
                                </span>
                            </div>
                            <div class="row" style="margin-top:10px;align-items:flex-start">
                                <span class="lbl">"URLs"</span>
                                <textarea
                                    class="input mono pair-urls"
                                    rows=move || (urls_text.get().lines().count() + 1).clamp(2, 12)
                                    spellcheck="false"
                                    aria-label="The URLs the device dials, one a line, in order"
                                    placeholder="https://192.0.2.1:8788"
                                    prop:value=move || urls_text.get()
                                    on:input=move |ev| urls_text.set(event_target_value(&ev))
                                ></textarea>
                            </div>
                            <div class="mini-note dim">
                                "One URL a line, in the order the device tries them. lmgw proposed these; add a name or an address the network rewrites, and it goes into the server certificates."
                            </div>
                            <Show when=move || none_found.get()>
                                <div class="mini-note notice warn">
                                    "lmgw found no address of that kind on this machine: type the URL the device dials."
                                </div>
                            </Show>
                            <Show when=move || !notes.get().is_empty() && !w.get().local_only()>
                                <div class="mini-note dim pair-url-notes">
                                    <For each=move || notes.get() key=|n| n.0.clone() let:n>
                                        <div>
                                            <span class="mono-sm">{n.0.clone()}</span>
                                            {format!(" — {}", n.1)}
                                        </div>
                                    </For>
                                </div>
                            </Show>
                            <AliasScopeEditor state=scope disabled=Signal::derive(|| false)/>
                            <ToolScopeEditor state=scope inventory=inv_state disabled=Signal::derive(|| false)/>
                            <DeviceToolsEditor
                                state=device_tools
                                hosted=hosted
                                own=Signal::derive(|| None::<i64>)
                                disabled=Signal::derive(|| false)
                                scope=Signal::derive(move || (scope.tool_mode.get(), scope.tool_patterns()))
                            />
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
                            <tr><th>"Device tools"</th><td class="mono-sm">{device_tools_words}</td></tr>
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
                                <th>"Where"</th>
                                <td>
                                    {move || w.get().label()}
                                    {move || {
                                        w.get().local_only().then_some(" — reachable from this computer only")
                                    }}
                                </td>
                            </tr>
                            <tr>
                                <th>"URLs"</th>
                                <td class="mono-sm pair-listing">{listing}</td>
                            </tr>
                        </tbody>
                    </table>
                    <Show when=move || !need_lines().is_empty()>
                        <div class="notice warn pair-need">
                            {move || {
                                need_lines().into_iter().map(|l| view! { <p>{l}</p> }).collect_view()
                            }}
                        </div>
                    </Show>
                    <Show when=wide>
                        <p class="notice warn">
                            "Every alias and no budget: a stolen key from this device spends on every alias without a ceiling. Go back and set a budget if a cloud alias is reachable."
                        </p>
                    </Show>
                    <Show when=open_door>
                        <p class="notice warn">
                            "Require API key is off: anyone who reaches this address uses /v1 without a key, and a Delete or New token of this device would leave its key working as no key at all. Switch it on under Settings → Network & access."
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
                        // Widening can be declined where the pairing can be
                        // written without it: the device then cannot reach
                        // lmgw (§4.1).
                        <Show when=move || need.get().is_some_and(|n| n == Need::Widen)>
                            <button
                                class="btn"
                                disabled=move || busy.get()
                                title=logic::DECLINED
                                on:click=move |_| pair(false)
                            >
                                "Pair without widening"
                            </button>
                        </Show>
                        <button class="btn primary" disabled=move || busy.get() on:click=move |_| pair(true)>
                            {move || {
                                if busy.get() {
                                    "Pairing…"
                                } else {
                                    match need.get() {
                                        Some(Need::Widen) => "Widen and pair the device",
                                        Some(Need::TurnOn { widen: true }) => "Turn on, widen and pair",
                                        Some(Need::TurnOn { widen: false }) => "Turn on and pair",
                                        _ => "Pair the device",
                                    }
                                }
                            }}
                        </button>
                    </Show>
                </ModalFooter>
            </div>
        </Modal>
    }
}
