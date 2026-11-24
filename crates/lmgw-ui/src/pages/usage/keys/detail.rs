//! A key's detail: its header bar (the name in a narrow window, its
//! requests in Traffic, the menu of what ends or changes it), the hero (the
//! kind's glyph, the name, what kind it is and who made it, chips for its
//! state, scope and a device's grants), the plaintext a create just
//! returned, its spend, its policy as preference rows ([`PolicyGroups`]), a
//! device's connection and tools ([`DeviceGroups`]), and what ends it as
//! rows of its own: copy and rotate an owner key, revoke a key, a device's
//! new token, disable and delete.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{KeyRow, KeySecret, KeysResponse};
use serde_json::json;

use super::super::devices::{DeviceChips, DeviceGroups, Ending};
use super::super::key_editor::{unsaved, PolicyForm, PolicyGroups};
use super::super::key_line::{
    bare_name, expiry_past, kind_icon, kind_tip, kind_word, limits_text, others_door, period_tip,
    reach, revocable, used_tip, used_words, SpendMeter,
};
use super::super::*;
use super::list::state_of;
use super::{Fresh, KeysCtx};
use crate::fmt::grouped;
use crate::widgets::nav_split::{track_scroll, DetailHead};
use crate::widgets::{
    use_dirty_guard, use_toasts, ActionRow, CopyBtn, Ico, Icon, MenuItem, PrefGroup, StackRow,
    StateChip,
};

/// The selected key, built once per id.
#[component]
pub(super) fn KeyDetail(
    page: KeysCtx,
    sel: Memo<Option<i64>>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    move || {
        sel.get()
            .map(|id| view! { <KeyPage page=page id=id narrow=narrow back=back/> })
    }
}

/// The plaintext a create just returned, with Copy, until dismissed. An
/// owner key stays copyable from its page, where Rotate is; a client key is
/// this line and then never again (principals §3.12).
#[component]
pub(super) fn FreshNote(page: KeysCtx, fresh: Fresh) -> impl IntoView {
    let kept = fresh.name.starts_with("owner:");
    let key = fresh.key.clone();
    view! {
        <div class="pref-group fresh-key">
            <div class="boxed">
                <div class="prow stack block">
                    <div class="prow-text">
                        <span class="prow-title">{format!("Key '{}' created", fresh.name)}</span>
                        <span class="prow-sub">
                            {if kept {
                                "It stays copyable from its page."
                            } else {
                                "Copy it now: it is not shown again."
                            }}
                        </span>
                    </div>
                    <div class="prow-block fresh-key-line">
                        <span class="mono-sm fresh-key-text">{key.clone()}</span>
                        <CopyBtn text=key title="Copy the key"/>
                        <button class="btn sm" on:click=move |_| page.fresh.set(None)>
                            "Dismiss"
                        </button>
                    </div>
                </div>
            </div>
        </div>
    }
}

#[component]
fn KeyPage(page: KeysCtx, id: i64, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let toasts = use_toasts();
    let keys = page.keys;
    let row = Memo::new(move |_| {
        keys.data.with(|d| {
            d.as_ref()
                .and_then(|k| k.keys.iter().find(|r| r.id == id).cloned())
        })
    });
    let loaded = Memo::new(move |_| keys.data.with(Option::is_some));
    let cur = Memo::new(move |_| {
        keys.data
            .with(|d| d.as_ref().map(|k| k.currency.clone()).unwrap_or_default())
    });
    let hosted = Signal::derive(move || {
        keys.data.with(|d| {
            d.as_ref()
                .map(|k| super::super::device_tools::hosted_labels(&k.keys))
                .unwrap_or_default()
        })
    });
    let kind = Memo::new(move |_| row.with(|r| r.as_ref().map(|r| r.kind.clone())));
    // The body is built once per key and kind; what it shows follows `row`.
    let present = Memo::new(move |_| row.with(Option::is_some));
    // A key just created is opened before the list has been read again: the
    // list as it stood when this page opened does not have it yet. Its
    // plaintext shows at once, and its absence is not taken for a revoke
    // until a list read after the create has come back.
    let just_made = Memo::new(move |_| page.fresh.with(|f| f.as_ref().is_some_and(|f| f.id == id)));
    let reread = RwSignal::new(false);
    Effect::new(move |prev: Option<()>| {
        keys.data.track();
        if prev.is_some() {
            reread.set(true);
        }
    });
    let fresh_note = move || {
        page.fresh
            .get()
            .filter(|f| f.id == id)
            .map(|f| view! { <FreshNote page=page fresh=f/> })
    };
    let form = PolicyForm::new(row, hosted, Callback::new(move |()| page.reload()));
    // Leaving the key with its policy unsaved asks.
    use_dirty_guard().watch_in(
        "this key",
        unsaved(form),
        Signal::stored(format!("/usage/keys/{id}")),
    );
    // A device deleted from here: its page is gone with it.
    Effect::new(move |_| {
        if page.devices.deleted.get() == Some(id) {
            page.devices.deleted.set(None);
            let q = super::list_query(&window().location().search().unwrap_or_default());
            page.go_unasked(&format!("/usage/keys{q}"), true);
        }
    });
    let scrolled = RwSignal::new(false);
    let title = Signal::derive(move || {
        row.with(|r| {
            r.as_ref()
                .map(|r| bare_name(&r.kind, &r.name).to_string())
                .unwrap_or_default()
        })
    });

    // Copy is a fetch, not a stored string: the plaintext is asked for on
    // click and lives for as long as it takes to paste it.
    let copy = move || {
        spawn_local(async move {
            let res =
                crate::api::post::<KeySecret, _>("/api/op/key_reveal", &json!({ "id": id })).await;
            match res {
                Ok(v) => crate::widgets::copy_secret(&v.key, toasts, "key copied".into()),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let copy = StoredValue::new(copy);
    // What ends or changes it, as the menu and the last group say them.
    let acts = Memo::new(move |_| row.get().map(|r| actions_of(&r)));
    let run = move |a: Act| {
        let Some(r) = row.get_untracked() else { return };
        let d = page.devices;
        match a {
            Act::Copy => copy.with_value(|c| c()),
            Act::Rotate => page.rotate.run(r),
            Act::Revoke => page.revoke.run(r),
            Act::NewToken => d.new_token.run(r),
            Act::Disable => d.ask.run((Ending::Disable, r)),
            Act::Enable => d.enable.run(r),
            Act::Delete => d.ask.run((Ending::Delete, r)),
        }
    };
    let menu = Signal::derive(move || {
        acts.get()
            .unwrap_or_default()
            .into_iter()
            .map(|a| MenuItem::new(a.menu_label(), move || run(a)).title(a.sub()))
            .collect::<Vec<_>>()
    });
    let traffic = move || TrafficLink::scoped("", "").key(id).href();

    let body = move || {
        if !present.get() {
            return view! {
                <div class="detail-flow">
                    {fresh_note}
                    <div class="pref-group">
                        <div class="boxed">
                            <div class="prow notice-row">
                                {move || {
                                    let waiting = just_made.get() && !reread.get();
                                    if keys.err.with(Option::is_some) && (waiting || !loaded.get()) {
                                        "The keys could not be read.".to_string()
                                    } else if waiting || !loaded.get() {
                                        "Loading…".to_string()
                                    } else {
                                        format!("No key {id}: it was revoked or deleted, or the link is old.")
                                    }
                                }}
                            </div>
                        </div>
                    </div>
                </div>
            }
            .into_any();
        }
        let device = kind.get().as_deref() == Some("device");
        view! {
            <div class="detail-flow kdetail">
                <Hero row=row keys=keys/>
                {fresh_note}
                <KindNote row=row/>
                <PrefGroup title="Spend">
                    <StackRow title=Signal::derive(move || {
                        row.with(|r| r.as_ref().map(|r| if r.budget_micro > 0 { "Spent against its budget" } else { "Spent" }).unwrap_or("Spent").to_string())
                    }) class="key-spend">
                        <SpendMeter row=row cur=cur/>
                    </StackRow>
                    <div class="prow">
                        <div class="prow-text">
                            <span class="prow-title">
                                {move || row.with(|r| period_tip(r.as_ref().map_or("", |r| r.budget_period.as_str())))}
                            </span>
                        </div>
                        <span class="prow-value">
                            {move || row.with(|r| grouped(r.as_ref().map_or(0, |r| r.requests.max(0)) as u64))}
                        </span>
                    </div>
                    <div class="prow" title=move || row.with(|r| used_tip(r.as_ref().and_then(|r| r.last_used.as_deref())))>
                        <div class="prow-text">
                            <span class="prow-title">"Last used"</span>
                        </div>
                        <span class="prow-value">
                            {move || row.with(|r| used_words(r.as_ref().and_then(|r| r.last_used.as_deref())))}
                        </span>
                    </div>
                    <a class="prow action" href=traffic>
                        <div class="prow-text">
                            <span class="prow-title">"Its requests in Traffic"</span>
                            <span class="prow-sub">"What it asked for, when, and what it cost"</span>
                        </div>
                        <Icon ico=Ico::Right class="prow-chevron"/>
                    </a>
                </PrefGroup>
                {device.then(|| view! { <DeviceGroups row=row keys=keys refresh=page.refresh acts=page.devices/> })}
                <PolicyGroups form=form hosted=hosted/>
                {move || {
                    let list = acts.get().unwrap_or_default();
                    (!list.is_empty()).then(|| {
                        view! {
                            <PrefGroup title="Rotate, revoke, delete">
                                {list
                                    .into_iter()
                                    .map(|a| {
                                        view! {
                                            <ActionRow
                                                title=a.label()
                                                sub=a.sub()
                                                danger=a.danger()
                                                on_click=Callback::new(move |()| run(a))
                                            />
                                        }
                                    })
                                    .collect_view()}
                            </PrefGroup>
                        }
                    })
                }}
            </div>
        }
        .into_any()
    };

    view! {
        <DetailHead
            narrow=narrow
            back=back
            title=title
            scrolled=scrolled
            more=menu
        />
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            {body}
        </div>
    }
}

/// What can be done to a key besides its policy, by kind.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Act {
    Copy,
    Rotate,
    Revoke,
    NewToken,
    Disable,
    Enable,
    Delete,
}

impl Act {
    fn label(self) -> &'static str {
        match self {
            Self::Copy => "Copy the key",
            Self::Rotate => "Rotate the key…",
            Self::Revoke => "Revoke the key…",
            Self::NewToken => "New token…",
            Self::Disable => "Disable the device…",
            Self::Enable => "Enable the device",
            Self::Delete => "Delete the device…",
        }
    }

    fn menu_label(self) -> &'static str {
        match self {
            Self::Copy => "Copy key",
            Self::Rotate => "Rotate…",
            Self::Revoke => "Revoke…",
            Self::NewToken => "New token…",
            Self::Disable => "Disable…",
            Self::Enable => "Enable",
            Self::Delete => "Delete…",
        }
    }

    fn sub(self) -> &'static str {
        match self {
            Self::Copy => "An owner key is stored, not shown once: copy it to the clipboard",
            Self::Rotate => "Replace it; anything holding the old one is locked out at once",
            Self::Revoke => "Delete it: anything presenting it is refused from now on",
            Self::NewToken => "Pair it again: a new link, keeping its current key until the new one is confirmed, or ending it now",
            Self::Disable => "Refuse every request it makes, and close its connections now",
            Self::Enable => "Let it in again with the key it has",
            Self::Delete => "Delete its key and policy; its spend stays in Usage",
        }
    }

    fn danger(self) -> bool {
        matches!(
            self,
            Self::Rotate | Self::Revoke | Self::Disable | Self::Delete
        )
    }
}

/// An owner key's own two (stored, so copyable; replaceable, behind a
/// question) unless it is another account's sign-in key, a revocable key's
/// Revoke, a device's three.
fn actions_of(r: &KeyRow) -> Vec<Act> {
    let mut v = Vec::new();
    match r.kind.as_str() {
        "owner" if !others_door(r) => {
            v.push(Act::Copy);
            v.push(Act::Rotate);
        }
        "device" => {
            v.push(Act::NewToken);
            v.push(if r.enabled { Act::Disable } else { Act::Enable });
            v.push(Act::Delete);
        }
        _ => {}
    }
    if revocable(r) {
        v.push(Act::Revoke);
    }
    v
}

/// The hero: the kind's glyph, the name, what it is, and chips for its
/// state and reach.
#[component]
fn Hero(row: Memo<Option<KeyRow>>, keys: Src<KeysResponse>) -> impl IntoView {
    let kind = row.with_untracked(|r| r.as_ref().map(|r| r.kind.clone()).unwrap_or_default());
    let name = move || {
        row.with(|r| {
            r.as_ref()
                .map(|r| bare_name(&r.kind, &r.name).to_string())
                .unwrap_or_default()
        })
    };
    let sub = move || {
        row.with(|r| {
            r.as_ref()
                .map(|r| {
                    let mut s = kind_word(&r.kind).to_string();
                    if bare_name(&r.kind, &r.name) != r.name {
                        s.push_str(&format!(" · {}", r.name));
                    }
                    if let Some(m) = &r.made_by {
                        s.push_str(&format!(
                            " · {} by {m}",
                            if r.kind == "device" { "paired" } else { "made" }
                        ));
                    }
                    s
                })
                .unwrap_or_default()
        })
    };
    let state = move || {
        let r = row.get()?;
        let expired = r.expires_at.as_deref().is_some_and(expiry_past);
        let (dot, word) = state_of(&r, expired);
        let tip = r.paused.clone().unwrap_or_default();
        Some(view! { <StateChip dot=dot word=word title=tip/> })
    };
    let reach_chips = move || {
        let r = row.get()?;
        if r.kind == "owner" {
            return Some(view! { <span class="chip">"Every capability"</span> }.into_any());
        }
        let mut v = vec![view! {
            <span class="chip" title="The model aliases it may ask for">
                {reach(&r.scope_mode, &r.scope_patterns, "alias")}
            </span>
        }
        .into_any()];
        if matches!(r.kind.as_str(), "key" | "device") {
            v.push(
                view! {
                    <span class="chip" title="The MCP tools it sees on /mcp">
                        {reach(&r.tool_scope_mode, &r.tool_scope_patterns, "tool")}
                    </span>
                }
                .into_any(),
            );
        }
        if r.kind != "internal" {
            v.push(view! { <span class="chip">{limits_text(&r)}</span> }.into_any());
        }
        if let Some(e) = r.expires_at.clone().filter(|e| !e.is_empty()) {
            v.push(view! { <span class="chip" title="It expires at the end of this day, or at this moment">{format!("Expires {e}")}</span> }.into_any());
        }
        Some(v.collect_view().into_any())
    };
    let is_device = kind == "device";
    view! {
        <div class="detail-hero">
            <div class="glyph" title=kind_tip(&kind)>
                <Icon ico=kind_icon(&kind)/>
            </div>
            <div class="hero-text">
                <h1 title=name>{name}</h1>
                <div class="sub" title=sub>{sub}</div>
                <div class="chips">
                    {state}
                    {reach_chips}
                    {is_device.then(|| view! { <DeviceChips row=row keys=keys/> })}
                    {(!is_device)
                        .then_some(move || {
                            let r = row.get()?;
                            let hosted = keys.data.with(|d| {
                                d.as_ref()
                                    .map(|k| super::super::device_tools::hosted_labels(&k.keys))
                                    .unwrap_or_default()
                            });
                            let unmet = super::super::device_tools::needs_grant(
                                &r.tool_scope_mode,
                                &r.tool_scope_patterns,
                                &r.device_tools,
                                None,
                                &hosted,
                            );
                            Some(super::super::device_tools::needs_grant_chips(unmet))
                        })}
                </div>
            </div>
        </div>
    }
}

/// What a kind's detail owns and does not, said once above its policy.
#[component]
fn KindNote(row: Memo<Option<KeyRow>>) -> impl IntoView {
    move || {
        let kind = row.with(|r| r.as_ref().map(|r| r.kind.clone()))?;
        let text = match kind.as_str() {
            "agent" => "An agent's token. Its scope is derived from the agent's manifest and rewritten on the next save, run or import, its tools are the manifest's tools[], and its enabled flag mirrors the agent: Disable there is the kill switch. Budget, rate limits and expiry are yours to set here, and they are enforced.",
            "internal" => "An internal identity: work lmgw does on its own behalf. It can never authenticate, so rate limits, expiry and a tool scope, which bind a credential it never presents, are greyed out. Scope and budget are enforced for it, and a budget is the lever that stops an unattended ingest.",
            "owner" => "An owner credential: the dashboard's session and the self-admin plane. It holds every capability, so it has no scope, no budget and no rate limit to set: an owner key is not a client.",
            _ => return None,
        };
        Some(view! {
            <div class="pref-group kind-note">
                <div class="boxed">
                    <div class="prow notice-row">{text}</div>
                </div>
            </div>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, name: &str) -> KeyRow {
        KeyRow {
            kind: kind.into(),
            name: name.into(),
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn each_kind_offers_what_is_its_owners_call() {
        assert_eq!(actions_of(&row("key", "ci")), vec![Act::Revoke]);
        assert_eq!(
            actions_of(&row("owner", "owner:mcp")),
            vec![Act::Copy, Act::Rotate, Act::Revoke]
        );
        assert_eq!(
            actions_of(&row("owner", "owner:dashboard")),
            vec![Act::Copy, Act::Rotate]
        );
        let mut d = row("device", "device:phone");
        assert_eq!(
            actions_of(&d),
            vec![Act::NewToken, Act::Disable, Act::Delete]
        );
        d.enabled = false;
        assert_eq!(
            actions_of(&d),
            vec![Act::NewToken, Act::Enable, Act::Delete]
        );
        assert!(actions_of(&row("agent", "agent:x")).is_empty());
        assert!(actions_of(&row("internal", "internal:x")).is_empty());
        let mut door = row("owner", "owner:alice");
        door.door_of = Some("alice".into());
        assert!(actions_of(&door).is_empty());
    }
}
