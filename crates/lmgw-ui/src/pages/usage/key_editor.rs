use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{KeyRow, ToolInventory};
use serde_json::{json, Value};

use super::scope_editor::{AliasScopeEditor, ScopeState, ToolScopeEditor};
use crate::widgets::{use_toasts, Modal, ModalFooter, Select};

#[component]
pub(super) fn KeyEditor(
    open: RwSignal<bool>,
    editing: RwSignal<Option<KeyRow>>,
    refresh: RwSignal<u32>,
) -> impl IntoView {
    let toasts = use_toasts();
    let scope = ScopeState::new();
    let budget = RwSignal::new(String::new());
    let period = RwSignal::new(String::new());
    let rpm = RwSignal::new(String::new());
    let tpm = RwSignal::new(String::new());
    let conc = RwSignal::new(String::new());
    let expiry = RwSignal::new(String::new());
    // A past expiry ends the key's connections the moment it is saved
    // (client-apps design §1.6): said under the field on the first Save, done
    // on the second (review W2-20). Any edit of the field asks again.
    let past_said = RwSignal::new(false);
    let stored_expiry = RwSignal::new(String::new());
    let note = RwSignal::new(String::new());
    let enabled = RwSignal::new(true);
    let hosts = RwSignal::new(String::new());

    // Which half of its own policy this row owns.
    //
    // An **agent** token's scope and enabled flag are *derived*: lmgw rewrites
    // them from the manifest and the agent row on every save, run or import
    // (container-runtime §3.1), so a value typed here would be taken back
    // silently on the next agent write. An **internal** identity never reaches
    // the auth middleware, which is where the rate limits and expiry are
    // checked, and it cannot authenticate at all — so those four are columns
    // nothing would ever read.
    //
    // Both are greyed out here *and* refused by `key_set`: the dialog
    // explains, the server is what enforces.
    //
    // An **owner** key is not a client at all (principals §3.12): it is not
    // scoped, budgeted or rate-limited, and `key_set` refuses every one of
    // those fields with `refuse_owner`. They are not greyed out here, they are
    // not shown — there is nothing to look at. `owner:dashboard` gives up one
    // more, its `enabled` flag: it is the door, and switching it off from
    // behind would lock the owner out of the surface that could switch it back.
    let kind = Memo::new(move |_| editing.get().map(|k| k.kind).unwrap_or_default());
    // A device's policy is a client key's (client-apps design §1.1), plus its
    // hosting label. Its enabled flag is not here: Disable is on the Devices
    // card, which says what it ends before it ends it.
    let device = Memo::new(move |_| kind.get() == "device");
    let client = Memo::new(move |_| matches!(kind.get().as_str(), "key" | "device"));
    let derived = Memo::new(move |_| kind.get() == "agent");
    let inert = Memo::new(move |_| kind.get() == "internal");
    let owner = Memo::new(move |_| kind.get() == "owner");
    let door = Memo::new(move |_| editing.get().is_some_and(|k| k.name == "owner:dashboard"));
    let agent_id = Memo::new(move |_| {
        editing
            .get()
            .map(|k| k.name.trim_start_matches("agent:").to_string())
            .unwrap_or_default()
    });

    // Load the row into the form whenever a different key is opened.
    Effect::new(move |_| {
        let Some(k) = editing.get() else { return };
        scope.load(
            &k.scope_mode,
            &k.scope_patterns,
            &k.tool_scope_mode,
            &k.tool_scope_patterns,
        );
        budget.set(if k.budget_micro > 0 {
            format!("{:.2}", k.budget_micro as f64 / 1e6)
        } else {
            String::new()
        });
        period.set(if k.budget_period.is_empty() {
            "month".into()
        } else {
            k.budget_period.clone()
        });
        rpm.set(if k.rpm_limit > 0 {
            k.rpm_limit.to_string()
        } else {
            String::new()
        });
        tpm.set(if k.tpm_limit > 0 {
            k.tpm_limit.to_string()
        } else {
            String::new()
        });
        conc.set(if k.concurrency_limit > 0 {
            k.concurrency_limit.to_string()
        } else {
            String::new()
        });
        expiry.set(k.expires_at.clone().unwrap_or_default());
        stored_expiry.set(k.expires_at.clone().unwrap_or_default());
        past_said.set(false);
        note.set(k.note.clone());
        enabled.set(k.enabled);
        hosts.set(k.hosts_label.clone().unwrap_or_default());
    });
    // What the gateway serves on `/mcp`, for the tool scope's preview — read
    // only while a client key's dialog is open, since listing connects every
    // enabled server that is not up yet (the MCP page does the same).
    let inventory = LocalResource::new(move || {
        let wanted = open.get() && client.get();
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
    // The preview runs the same matcher the gate does, from the shared crate,
    // so what it lists is what the key will be served.
    let preview = Memo::new(move |_| {
        let inv = inv_state.get()?.ok()?;
        let offered: Vec<String> = inv
            .tools
            .iter()
            .filter(|t| t.plane == "/mcp" && t.available && !t.stale)
            .map(|t| t.name.clone())
            .collect();
        let (mode, pats) = (scope.tool_mode.get(), scope.tool_patterns());
        let seen: Vec<String> = offered
            .iter()
            .filter(|n| lmgw_api_types::scope::admits(&mode, &pats, n))
            .cloned()
            .collect();
        Some((seen, offered.len()))
    });
    // One send for both shapes of body below.
    let submit = move |body: Value| {
        spawn_local(async move {
            match crate::api::post::<Value, _>("/api/op/key_set", &body).await {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("key policy saved")
                        .to_string());
                    open.set(false);
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let save = move |_| {
        let Some(k) = editing.get_untracked() else {
            return;
        };
        // An owner row owns two fields. The rest are not sent to be refused —
        // they are not sent.
        if k.kind == "owner" {
            let mut body = json!({ "id": k.id, "note": note.get_untracked() });
            if k.name != "owner:dashboard" {
                body["enabled"] = json!(enabled.get_untracked());
            }
            submit(body);
            return;
        }
        // A field that will not parse is refused out loud. Reading it as `0`
        // — which is what this did — turns "10,50" typed by anyone who writes
        // decimals with a comma into *no budget at all*, saved with a green
        // toast.
        let limit = |sig: RwSignal<String>, label: &str| -> Option<i64> {
            let raw = sig.get_untracked();
            let raw = raw.trim();
            if raw.is_empty() {
                return Some(0);
            }
            match raw.parse::<i64>() {
                Ok(v) if v >= 0 => Some(v),
                _ => {
                    toasts.err(format!(
                        "{label} must be a whole number, 0 or empty for none"
                    ));
                    None
                }
            }
        };
        let raw_budget = budget.get_untracked();
        let raw_budget = raw_budget.trim().replace(',', ".");
        let budget_micro = if raw_budget.is_empty() {
            0
        } else {
            match raw_budget.parse::<f64>() {
                Ok(v) if v >= 0.0 => (v * 1e6).round() as i64,
                _ => {
                    toasts.err("budget must be an amount like 10 or 10.50 — empty means none");
                    return;
                }
            }
        };
        let (Some(rpm_v), Some(tpm_v), Some(conc_v)) = (
            limit(rpm, "requests / min"),
            limit(tpm, "tokens / min"),
            limit(conc, "concurrency"),
        ) else {
            return;
        };
        let typed = expiry.get_untracked();
        if typed.trim() != stored_expiry.get_untracked().trim()
            && expiry_past(typed.trim())
            && !past_said.get_untracked()
        {
            past_said.set(true);
            return;
        }
        let mut body = json!({
            "id": k.id,
            "enabled": enabled.get_untracked(),
            "scope_mode": scope.scope_mode.get_untracked(),
            "scope_patterns": untrack(|| scope.alias_patterns()),
            "tool_scope_mode": scope.tool_mode.get_untracked(),
            "tool_scope_patterns": untrack(|| scope.tool_patterns()),
            "budget_micro": budget_micro,
            "budget_period": period.get_untracked(),
            "rpm_limit": rpm_v,
            "tpm_limit": tpm_v,
            "concurrency_limit": conc_v,
            "expires_at": expiry.get_untracked().trim(),
            "note": note.get_untracked(),
        });
        if k.kind == "device" {
            // Disable lives on the Devices card, behind its confirmation.
            if let Some(o) = body.as_object_mut() {
                o.remove("enabled");
            }
            body["hosts_label"] = json!(hosts.get_untracked().trim());
        }
        submit(body);
    };
    view! {
        <Modal open=open title="Key policy" guard=true>
            <div class="form">
                <div class="tile-label">
                    {move || editing.get().map(|k| k.name).unwrap_or_default()}
                </div>
                {move || {
                    derived
                        .get()
                        .then(|| {
                            let id = agent_id.get();
                            view! {
                                <div class="mini-note dim" style="margin-top:6px">
                                    "This is an agent's token. Its "
                                    <b>"scope"</b>
                                    " is derived from the agent's manifest and rewritten on the next save, run or import, its "
                                    <b>"tools"</b>
                                    " are the manifest's tools[], and its "
                                    <b>"enabled"</b>
                                    " flag mirrors the agent — Disable there is the kill switch. Both are shown here and set on "
                                    <a href=format!("/agents/{id}")>"the agent's page"</a>
                                    ". Budget, rate limits and expiry are yours to set here, and they are enforced."
                                </div>
                            }
                        })
                }}
                {move || {
                    inert
                        .get()
                        .then(|| {
                            view! {
                                <div class="mini-note dim" style="margin-top:6px">
                                    "This is an internal identity — work lmgw does on its own behalf. It can never authenticate, and rate limits, expiry and a tool scope bind a credential it never presents, so those are greyed out: they would be stored and never read. "
                                    <b>"Scope and budget are enforced"</b>
                                    " for it, and a budget is the lever that stops an unattended ingest."
                                </div>
                            }
                        })
                }}
                {move || {
                    owner
                        .get()
                        .then(|| {
                            view! {
                                <div class="mini-note dim" style="margin-top:6px">
                                    "This is an owner credential — the dashboard's session and the "
                                    "self-admin plane. It holds every capability, so it has no "
                                    "scope, no budget and no rate limit to set: "
                                    <b>"an owner key is not a client"</b>
                                    ". Copy and Rotate are on its row."
                                </div>
                            }
                        })
                }}
                <label class="row" style="margin-top:10px">
                    <input
                        type="checkbox"
                        prop:checked=move || enabled.get()
                        disabled=move || derived.get() || inert.get() || door.get() || device.get()
                        title=move || {
                            if door.get() { "the dashboard key is the door; rotate it instead" }
                            else if device.get() { "Disable a device on its row under Devices, which says what it ends" }
                            else { "" }
                        }
                        on:change=move |ev| enabled.set(event_target_checked(&ev))
                    />
                    "enabled"
                </label>
                // Everything a client key has and an owner key does not.
                <Show when=move || !owner.get()>
                    <AliasScopeEditor state=scope disabled=Signal::derive(move || derived.get())/>
                    <ToolScopeEditor
                        state=scope
                        inventory=inv_state
                        disabled=Signal::derive(move || derived.get() || inert.get())
                    />
                    <Show when=move || client.get()>
                        <div class="mini-note dim">
                            {move || match preview.get() {
                                None => "Which MCP tools this key sees on /mcp and may attach in a /v1/responses run.".to_string(),
                                Some((seen, of)) if seen.len() == of => {
                                    format!("Sees all {of} tools on /mcp.")
                                }
                                Some((seen, of)) if seen.is_empty() => {
                                    format!("Sees none of the {of} tools on /mcp.")
                                }
                                Some((seen, of)) => {
                                    format!("Sees {} of {of} tools on /mcp: {}.", seen.len(), seen.join(", "))
                                }
                            }}
                            " The lmgw__* self-admin tools need an owner key and are never in reach of a client key. A caller with no key sees everything until Require API key is on."
                        </div>
                    </Show>
                    <Show when=move || device.get()>
                        <div class="row" style="margin-top:10px">
                            <span class="lbl">"hosts tools as"</span>
                            <input
                                class="input mono"
                                style="max-width:180px"
                                placeholder="no grant"
                                prop:value=move || hosts.get()
                                on:input=move |ev| hosts.set(event_target_value(&ev))
                            />
                            <span class="dim mini-note">
                                "the prefix its own MCP tools are named under (label__…); empty takes the grant away"
                            </span>
                        </div>
                    </Show>
                    <div class="row" style="margin-top:10px">
                        <span class="lbl">"budget"</span>
                        <input
                            class="input mono"
                            style="max-width:110px"
                            placeholder="0 = none"
                            prop:value=move || budget.get()
                            on:input=move |ev| budget.set(event_target_value(&ev))
                        />
                        <Select
                            value=period
                            options=Signal::derive(|| {
                                vec![
                                    ("day".to_string(), "per day".to_string()),
                                    ("month".into(), "per month".into()),
                                    ("total".into(), "total".into()),
                                ]
                            })
                        />
                    </div>
                    <div style="display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin-top:10px">
                        <div>
                            <div class="lbl" title="counts model calls: each call of a Chat turn's tool loop, and a knowledge base's search, is one">"requests / min"</div>
                            <input
                                class="input mono"
                                style="width:100%"
                                placeholder="0 = none"
                                title="counts model calls: each call of a Chat turn's tool loop, and a knowledge base's search, is one"
                                disabled=move || inert.get()
                                prop:value=move || rpm.get()
                                on:input=move |ev| rpm.set(event_target_value(&ev))
                            />
                        </div>
                        <div>
                            <div class="lbl">"tokens / min"</div>
                            <input
                                class="input mono"
                                style="width:100%"
                                placeholder="0 = none"
                                disabled=move || inert.get()
                                prop:value=move || tpm.get()
                                on:input=move |ev| tpm.set(event_target_value(&ev))
                            />
                        </div>
                        <div>
                            <div class="lbl">"concurrency"</div>
                            <input
                                class="input mono"
                                style="width:100%"
                                placeholder="0 = none"
                                disabled=move || inert.get()
                                prop:value=move || conc.get()
                                on:input=move |ev| conc.set(event_target_value(&ev))
                            />
                        </div>
                    </div>
                    <div class="mini-note dim">
                        "A rate limit answers 429 with a real Retry-After; a budget answers 403, because a monthly budget will not clear inside a retry window."
                    </div>
                    <div class="row" style="margin-top:10px">
                        <span class="lbl">"expires"</span>
                        <input
                            class="input mono"
                            style="max-width:180px"
                            placeholder="empty = never"
                            disabled=move || inert.get()
                            prop:value=move || expiry.get()
                            on:input=move |ev| {
                                past_said.set(false);
                                expiry.set(event_target_value(&ev))
                            }
                        />
                        <span class="dim mini-note">
                            "YYYY-MM-DD — the whole of that day — or an RFC3339 timestamp. An expired key is 401 key_expired."
                        </span>
                    </div>
                    <Show when=move || past_said.get()>
                        <p class="notice warn">
                            "That moment has passed: saving expires the key now — its Chat feed, voice sessions and tool link close, and every request it makes is refused. Save again to do it."
                        </p>
                    </Show>
                </Show>
                <input
                    class="input"
                    style="margin-top:10px;width:100%"
                    placeholder="note"
                    prop:value=move || note.get()
                    on:input=move |ev| note.set(event_target_value(&ev))
                />
                <ModalFooter>
                    <button class="btn ghost" on:click=move |_| open.set(false)>
                        "Cancel"
                    </button>
                    <button class="btn primary" on:click=save>
                        "Save"
                    </button>
                </ModalFooter>
            </div>
        </Modal>
    }
}

/// Is `when` — the field's `YYYY-MM-DD` (the end of that day) or RFC 3339
/// timestamp — already past? `false` for empty or unreadable text, which the
/// server refuses or ignores itself.
fn expiry_past(when: &str) -> bool {
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
