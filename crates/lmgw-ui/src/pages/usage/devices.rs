//! The paired devices on Usage → Keys (client-apps design §1.4, §11 Q6): a
//! device is a row of the keys list under Devices, its detail the hero's
//! chips ([`DeviceChips`]), its connection and hosted tools
//! ([`DeviceGroups`]) and the three actions that end its connections —
//! each confirmed with what it ends, then done ([`DeviceActs`], whose
//! dialogs the page places). Pairing, the link strip and the transfers are
//! on the status page. And the device's level of lmgw's admin tools
//! (L3/L5, 2026-10-07; a level since the pre-merge review's P-3): off, read
//! only or full, set on the pairing form and in a device's detail,
//! confirmed with what the level lets the device do whenever it is raised
//! or set to read only.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::device_link::DevicePairingState;
use lmgw_api_types::{KeyRow, KeySaved, KeysResponse, MessageAck};
use serde_json::json;

// The pairing form says a scope in words too.
use super::key_line::reach;
use super::*;
use crate::widgets::{use_toasts, InfoRow, Modal, ModalFooter, PrefGroup, StackRow};

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
/// The transfers lmgw relays between devices (device transfers design
/// §11.1).
mod transfers;

pub(super) use logic::parse_urls;
use notes::PairingNotes;
use pair::PairDevice;
use qr_card::{QrCard, QrSession};
pub(super) use strip::LinkStrip;
use token_dialog::TokenDialog;
pub(super) use transfers::TransfersSection;

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
pub(super) enum Ending {
    Disable,
    Delete,
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

/// What a device's detail and the list's add menu reach: pairing, and the
/// actions that end or change a device's connections. Each opens its
/// question; the dialogs are the page's (see [`device_acts`]).
#[derive(Clone, Copy)]
pub(super) struct DeviceActs {
    /// The pairing form.
    pub pair_open: RwSignal<bool>,
    /// Disable or Delete, behind the question that says what it ends.
    pub ask: Callback<(Ending, KeyRow)>,
    /// Let a disabled device in again, at once.
    pub enable: Callback<KeyRow>,
    /// New token…, with its choices.
    pub new_token: Callback<KeyRow>,
    /// Set the admin-tools level: read only and full after a confirmation,
    /// off at once.
    pub admin_tools: Callback<(KeyRow, &'static str)>,
    /// A raise was asked about and not done (Cancel, or the save failed):
    /// the level's dropdown goes back to what the device has. A level that
    /// was applied stays shown until the re-read brings it.
    pub admin_asked: RwSignal<u32>,
    /// A Delete landed for this id.
    pub deleted: RwSignal<Option<i64>>,
}

/// The device actions and their dialogs (the pairing form, the QR card,
/// New token…, the admin-tools question, Disable and Delete), for the page
/// to place once.
pub(super) fn device_acts(
    keys: Src<KeysResponse>,
    refresh: RwSignal<u32>,
) -> (DeviceActs, impl IntoView) {
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
    let granting = RwSignal::new(None::<(KeyRow, &'static str)>);
    let grant_open = RwSignal::new(false);
    let admin_asked = RwSignal::new(0u32);
    let deleted = RwSignal::new(None::<i64>);
    // A grant question closed without its yes (Cancel, Escape) puts the
    // dropdown back. Its yes takes the question (`granting`) before it
    // closes, so the dropdown keeps the level it applied until the re-read
    // brings it.
    Effect::new(move |was: Option<bool>| {
        let open = grant_open.get();
        if was == Some(true) && !open && granting.get_untracked().is_some() {
            granting.set(None);
            admin_asked.update(|n| *n = n.wrapping_add(1));
        }
        open
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
            match res {
                Ok(_) => {
                    // The question closes on its yes, done.
                    granting.set(None);
                    grant_open.set(false);
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
                Err(e) => {
                    granting.set(None);
                    grant_open.set(false);
                    admin_asked.update(|n| *n = n.wrapping_add(1));
                    toasts.err(e.to_string());
                }
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
            busy.try_set(false);
            ask_open.try_set(false);
            match res {
                Ok(()) => {
                    toasts.ok(match what {
                        Ending::Disable => format!("device '{}' disabled", bare(&r.name)),
                        Ending::Delete => format!("device '{}' deleted", bare(&r.name)),
                    });
                    if what == Ending::Delete {
                        deleted.try_set(Some(r.id));
                    }
                    refresh.try_update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let acts = DeviceActs {
        pair_open,
        ask,
        enable,
        new_token,
        admin_tools,
        admin_asked,
        deleted,
    };
    let dialogs = view! {
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
    };
    (acts, dialogs)
}

/// The labels the paired devices host now.
fn hosted_now(keys: Src<KeysResponse>) -> Vec<super::device_tools::HostedLabel> {
    keys.data.with(|d| {
        d.as_ref()
            .map(|k| super::device_tools::hosted_labels(&k.keys))
            .unwrap_or_default()
    })
}

/// A device's chips for its hero: the app it declared, the label it hosts
/// tools under, each other device's tools it may use (a label no paired
/// device hosts now in amber), a scope pattern that needs a grant, its
/// admin-tools level, wide open, paired before TLS.
#[component]
pub(super) fn DeviceChips(row: Memo<Option<KeyRow>>, keys: Src<KeysResponse>) -> impl IntoView {
    let chips = move || {
        let Some(r) = row.get() else {
            return Vec::new();
        };
        let mut v: Vec<AnyView> = Vec::new();
        // The app id its enrolment declared (device transfers design §2):
        // what other devices of the same app offer files to.
        if let Some(app) = r.app_id.clone() {
            v.push(view! {
                <span class="chip info mono-sm" title="the app this install declared when it paired: other devices of the same app offer it files">
                    {format!("app {app}")}
                </span>
            }
            .into_any());
        }
        // The hosting grant (§1.5): the prefix its own tools are named under.
        if let Some(l) = r.hosts_label.clone() {
            let who = r.reaches.join(", ");
            v.push(view! {
                <span class="chip info mono-sm" title=format!("may host its own MCP tools, named {l}__… — reached by: {who}")>
                    {format!("{l}__")}
                </span>
            }
            .into_any());
        }
        let hosted = hosted_now(keys);
        // Its device-tools grant (client-apps design L16 as changed
        // 2026-10-15).
        for l in &r.device_tools {
            let host = hosted
                .iter()
                .find(|h| h.label.eq_ignore_ascii_case(l))
                .map(|h| h.device.clone());
            v.push(match host {
                Some(device) => view! {
                    <span class="chip info mono-sm" title=format!("may use the tools device '{device}' hosts, named {l}__…, as its tool scope picks them")>
                        {format!("uses {l}__")}
                    </span>
                }
                .into_any(),
                None => view! {
                    <span class="chip warn mono-sm" title="granted, but no paired device hosts tools under this label now: the grant waits for one that does">
                        {format!("{l}__: no device hosts it")}
                    </span>
                }
                .into_any(),
            });
        }
        let unmet = super::device_tools::needs_grant(
            &r.tool_scope_mode,
            &r.tool_scope_patterns,
            &r.device_tools,
            Some(r.id),
            &hosted,
        );
        v.push(super::device_tools::needs_grant_chips(unmet).into_any());
        if admin_on(r.self_admin.as_str()) {
            let level = admin_level_words(r.self_admin.as_str());
            v.push(view! {
                <span class="chip warn" title=format!("lmgw's admin tools at {level}: the Chat threads and folders with the self-admin toolset; the self-admin level set in Settings caps it")>
                    {format!("admin tools: {level}")}
                </span>
            }
            .into_any());
        }
        if wide_open(&r) {
            v.push(view! {
                <span class="chip warn" title="Every alias and no budget: a stolen device key spends on every alias without a ceiling. Set a budget where a cloud alias is reachable.">
                    "every alias · no budget"
                </span>
            }
            .into_any());
        }
        // A pairing from before TLS: its old key opens nothing (§12).
        if r.pairing
            .as_ref()
            .is_some_and(|p| p.state == DevicePairingState::V1)
        {
            v.push(view! {
                <span class="chip warn" title="Paired with a link from before TLS: its old key opens nothing. New token… pairs it again.">
                    "v1: pair again"
                </span>
            }
            .into_any());
        }
        v
    };
    view! { {chips} }
}

/// Where a device is now: its open connections, else when it was last seen.
fn seen_words(r: &KeyRow) -> (String, String) {
    if !r.online.is_empty() {
        return (
            format!("Online ({})", online_text(r)),
            "Its open connections now".into(),
        );
    }
    let seen = r
        .last_seen_at
        .as_deref()
        .map(|t| js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_str(t)).get_time())
        .filter(|ms| !ms.is_nan());
    match seen {
        Some(ms) => (
            format!("Last seen {}", crate::fmt::rel_time(ms / 1000.0)),
            crate::fmt::local_datetime(ms / 1000.0),
        ),
        None => ("Never connected".into(), String::new()),
    }
}

/// A device's own groups in its detail: its connection (state, who paired
/// it, the pairing and its next step, the certificate, the open links) and
/// what it hosts and may do (who reaches its tools, its admin-tools level).
#[component]
pub(super) fn DeviceGroups(
    row: Memo<Option<KeyRow>>,
    keys: Src<KeysResponse>,
    refresh: RwSignal<u32>,
    acts: DeviceActs,
) -> impl IntoView {
    let id = row.with_untracked(|r| r.as_ref().map_or(0, |r| r.id));
    let state = Memo::new(move |_| row.with(|r| r.as_ref().map(seen_words).unwrap_or_default()));
    let auth_off = move || {
        keys.data
            .with(|d| d.as_ref().is_some_and(|k| !k.auth_enabled))
    };
    // The admin-tools level as a dropdown that applies at once: off at once,
    // a raise or read only after its question; a question left without its
    // yes puts the dropdown back.
    let level = RwSignal::new(String::new());
    let has = Memo::new(move |_| {
        row.with(|r| r.as_ref().map(|r| r.self_admin.as_str().to_string()))
            .unwrap_or_default()
    });
    Effect::new(move |_| {
        acts.admin_asked.track();
        let h = has.get();
        if level.get_untracked() != h {
            level.set(h);
        }
    });
    Effect::new(move |_| {
        let want = level.get();
        let now = has.get_untracked();
        if want.is_empty() || want == now {
            return;
        }
        let Some(r) = row.get_untracked() else { return };
        let lv: &'static str = match want.as_str() {
            "full" => "full",
            "read_only" => "read_only",
            _ => "off",
        };
        acts.admin_tools.run((r, lv));
    });
    let level_opts = Signal::derive(|| {
        vec![
            ("off".to_string(), "Off".to_string()),
            ("read_only".into(), "Read only".into()),
            ("full".into(), "Full".into()),
        ]
    });
    let made_by = move || row.with(|r| r.as_ref().and_then(|r| r.made_by.clone()));
    let reached = move || {
        row.with(|r| {
            r.as_ref()
                .filter(|r| r.hosts_label.is_some())
                .map(|r| r.reaches.join(", "))
        })
    };
    view! {
        <PrefGroup title="Connection">
            <Show when=auth_off>
                <div class="prow notice-row warn devices-auth">
                    "Require API key is off: anyone who reaches this gateway's address uses /v1 "
                    "without a key, whatever this device's scope and budget say. Switch it on under "
                    <a href="/settings">"Settings → Network & access"</a>
                    "."
                </div>
            </Show>
            <div class="prow" title=move || state.get().1>
                <div class="prow-text">
                    <span class="prow-title">"State"</span>
                </div>
                <span class="prow-value" class:dev-online=move || row.with(|r| r.as_ref().is_some_and(|r| !r.online.is_empty()))>
                    {move || state.get().0}
                </span>
            </div>
            {move || made_by().map(|m| view! { <InfoRow title="Paired by" value=m/> })}
            <StackRow title="Pairing" class="dev-notes">
                <PairingNotes id=id row=row refresh=refresh/>
            </StackRow>
        </PrefGroup>
        <PrefGroup title="Hosted tools and admin tools">
            <InfoRow
                title="Its tools are reached by"
                value=Signal::derive(move || {
                    reached().map_or_else(|| "It hosts no tools (no label under Pairing)".to_string(), |w| if w.is_empty() { "nobody yet".to_string() } else { w })
                })
            />
            <div class="prow combo dev-admin">
                <div class="prow-text">
                    <span class="prow-title">"lmgw's admin tools"</span>
                    <span class="prow-sub">
                        "The Chat threads and folders with the self-admin toolset; the self-admin level set in Settings caps it. Read only and Full ask first"
                    </span>
                </div>
                <div class="prow-end">
                    <span class="applies-now">"applies now"</span>
                    <crate::widgets::Select value=level options=level_opts/>
                </div>
            </div>
        </PrefGroup>
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
}
