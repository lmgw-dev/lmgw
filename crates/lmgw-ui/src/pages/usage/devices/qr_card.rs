//! The QR card (device link design §3.1, §3.3, §3.5, §9.3): the pairing link
//! as a QR code and as text, with its expiry; when the device enrols it turns
//! into the confirmation (the device's code, Confirm, or the code typed from
//! the device for a contested row); closing it asks whether to end the
//! pairing code, the default ending it. The link lives here, in memory, and
//! is gone when the card is.

use leptos::task::spawn_local;
use lmgw_api_types::{DevicePaired, KeysResponse, MessageAck};
use serde_json::json;

use super::logic::{self, Phase};
use super::notes::{end_token, local_clock, ConfirmAction};
use super::*;
use crate::widgets::{copy_secret, use_toasts, Modal, ModalFooter};

/// A pairing link just minted (pair, or a new token): shown here once.
#[derive(Clone, PartialEq)]
pub(super) struct QrSession {
    id: i64,
    name: String,
    link: String,
    urls: Vec<String>,
    local_only: bool,
    /// When the token lapses (RFC 3339), which identifies it in the list.
    expires: String,
    fp: String,
    message: String,
    /// A new token for a device that was paired, not a first pairing.
    rotated: bool,
}

impl QrSession {
    pub(super) fn from_answer(v: DevicePaired, rotated: bool) -> Self {
        Self {
            id: v.id,
            name: bare(&v.name).to_string(),
            link: v.link,
            urls: v.urls,
            local_only: v.local_only,
            expires: v.token_expires_at,
            fp: v.fp,
            message: v.message,
            rotated,
        }
    }
}

/// The QR card, and the question closing it asks. `session` holds the
/// link; `open` is the card.
#[component]
pub(super) fn QrCard(
    session: RwSignal<Option<QrSession>>,
    open: RwSignal<bool>,
    keys: Src<KeysResponse>,
    refresh: RwSignal<u32>,
) -> impl IntoView {
    let toasts = use_toasts();
    let Now(now) = expect_context::<Now>();
    let row = Memo::new(move |_| {
        let id = session.with(|s| s.as_ref().map(|s| s.id))?;
        keys.data.with(|d| {
            d.as_ref()
                .and_then(|k| k.keys.iter().find(|r| r.id == id).cloned())
        })
    });
    // Whether the list has shown the row since the card opened: a row that
    // was there and is gone was deleted, and the card has nothing left to say.
    let seen = RwSignal::new(false);
    Effect::new(move |_| {
        if row.get().is_some() {
            seen.set(true);
        }
    });
    let phase = Memo::new(move |_| {
        let Some(s) = session.get() else {
            return Phase::Waiting;
        };
        row.with(|r| logic::card_phase(r.as_ref().and_then(|r| r.pairing.as_ref()), &s.expires))
    });
    let alive = move || {
        row.with(|r| {
            r.as_ref()
                .and_then(|r| r.pairing.as_ref())
                .and_then(|p| p.token.as_ref())
                .is_some_and(|t| t.alive)
        })
    };

    // Closing the card, however it closes, asks about the code while it
    // lives.
    let question = RwSignal::new(false);
    let answered = RwSignal::new(false);
    let deleted = RwSignal::new(false);
    Effect::new(move |was: Option<bool>| {
        let o = open.get();
        if was == Some(true) && !o {
            if session.get_untracked().is_none() {
                return o;
            }
            if alive() && !deleted.get_untracked() {
                answered.set(false);
                question.set(true);
            } else {
                session.set(None);
            }
        }
        o
    });
    // The row went (deleted from its menu, or by the card's own Delete).
    Effect::new(move |_| {
        if seen.get() && row.get().is_none() && open.get_untracked() {
            deleted.set(true);
            open.set(false);
        }
    });
    // The link is forgotten the moment the card and its question are gone.
    Effect::new(move |was: Option<bool>| {
        let q = question.get();
        if was == Some(true) && !q {
            if !answered.get_untracked() {
                // Dismissed without an answer: the default, end it.
                if let Some(s) = session.get_untracked() {
                    end_token(s.id, refresh, toasts, |_| {});
                }
            }
            session.set(None);
            seen.set(false);
            deleted.set(false);
        }
        q
    });
    Effect::new(move |was: Option<Option<String>>| {
        // A new session starts unseen and undeleted.
        let s = session.get().map(|s| s.expires);
        if was.as_ref() != Some(&s) {
            seen.set(false);
            deleted.set(false);
        }
        s
    });

    let end_now = move |_| {
        answered.set(true);
        question.set(false);
        if let Some(s) = session.get_untracked() {
            end_token(s.id, refresh, toasts, |_| {});
        }
    };
    let keep = move |_| {
        answered.set(true);
        question.set(false);
    };

    let body = move || {
        let s = session.get()?;
        let phase = phase.get();
        Some(match phase {
            Phase::Waiting => waiting(&s, now, toasts).into_any(),
            Phase::Confirm(a) => confirming(&s, a, refresh, deleted, open, toasts).into_any(),
            Phase::Paired => view! {
                <p>
                    <b>{format!("Device '{}' is paired.", s.name)}</b>
                    " It is served from its next request."
                </p>
                <p class="dim mini-note">
                    "Its pairing code stays alive until the device's first request with its certificate, or until you end it: closing this card asks."
                </p>
            }
            .into_any(),
            Phase::Ended => view! {
                <p class="notice warn">
                    {format!(
                        "The pairing code of device '{}' ended before the device used it (it lapsed, or was ended). The link pairs nothing.",
                        s.name,
                    )}
                </p>
                <p class="dim mini-note">
                    "Close this card, then choose New token… on the device's row for a new link."
                </p>
            }
            .into_any(),
        })
    };
    let until = move || {
        session
            .get()
            .map(|s| local_clock(&s.expires, now.get_untracked()))
            .unwrap_or_default()
    };
    view! {
        <Modal open=open title="Pair the device">{body}</Modal>
        <Modal open=question title="End the pairing code?">
            <p>
                "The code in the link you just showed is still valid. Ending it means nothing pairs with that link any more; a device that has not enrolled yet needs a new token."
            </p>
            <p class="dim mini-note">
                "Closing this question ends it too: ending is the default."
            </p>
            <ModalFooter>
                <button class="btn ghost" on:click=keep>
                    {move || format!("Keep it valid until {}", until())}
                </button>
                <button class="btn primary" on:click=end_now>
                    "End the pairing code now"
                </button>
            </ModalFooter>
        </Modal>
    }
}

/// The code to scan, its text and its expiry.
fn waiting(s: &QrSession, now: RwSignal<f64>, toasts: crate::widgets::Toasts) -> impl IntoView {
    let svg = super::qr::svg_of(&s.link, 232);
    let copy_link = s.link.clone();
    let expires = s.expires.clone();
    let left = move || {
        let n = now.get();
        match logic::minutes_left(&expires, n) {
            Some(m) => format!("valid until {} · {m} min left", local_clock(&expires, n)),
            None => format!(
                "lapsed at {}: close this card and make a new token",
                local_clock(&expires, n)
            ),
        }
    };
    let qr = match svg {
        Ok(svg) => view! {
            <div class="qr" role="img" aria-label="Pairing QR code" inner_html=svg></div>
        }
        .into_any(),
        Err(why) => view! {
            <div class="notice warn qr-fail">{why}". Use the link below."</div>
        }
        .into_any(),
    };
    let urls = s.urls.join(", ");
    view! {
        <p>
            {if s.rotated { "A new pairing link for device " } else { "Device " }}
            <b>{format!("'{}'", s.name)}</b>
            {if s.rotated { "." } else { " is created." }}
            " Scan the code with the device, or send it the link."
        </p>
        <div class="qr-row">
            {qr}
            <div class="qr-side">
                <div class="qr-expiry">{left}</div>
                <div class="mini-note dim">
                    "It reaches lmgw at " <span class="mono-sm">{urls}</span>
                    {s.local_only.then_some(" (this computer only)")}
                </div>
                <div class="mini-note dim">
                    "CA fingerprint "
                    <span class="mono-sm" title=s.fp.clone()>{logic::fp_short(&s.fp)}</span>
                    " is in the link: the device pins it."
                </div>
                <div class="mini-note">{s.message.clone()}</div>
            </div>
        </div>
        <div class="pair-link mono-sm">{s.link.clone()}</div>
        <div class="row" style="margin-top:10px">
            <button
                class="btn"
                on:click=move |_| copy_secret(&copy_link, toasts, "pairing link copied".into())
            >
                "Copy link"
            </button>
            <a class="btn" href=s.link.clone()>
                "Open on this computer"
            </a>
        </div>
        <p class="dim mini-note">
            "The link is a credential while its code lives: lmgw shows it only now and keeps its hash. Open starts the client app registered for lmgw-pair: links. The device then shows a code to compare here."
        </p>
    }
}

/// The device enrolled: its code, Confirm, or Delete.
fn confirming(
    s: &QrSession,
    a: logic::Awaiting,
    refresh: RwSignal<u32>,
    deleted: RwSignal<bool>,
    open: RwSignal<bool>,
    toasts: crate::widgets::Toasts,
) -> impl IntoView {
    let id = s.id;
    let name = s.name.clone();
    let from = a
        .from
        .clone()
        .map(|f| format!(" from {f}"))
        .unwrap_or_default();
    let contested = a.contested_from.clone();
    let replaces = a.replaces;
    let deleting = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let delete = move |_| {
        busy.set(true);
        spawn_local(async move {
            let res =
                crate::api::post::<MessageAck, _>("/api/op/key_delete", &json!({ "id": id })).await;
            busy.set(false);
            match res {
                Ok(_) => {
                    toasts.ok("device deleted".to_string());
                    deleted.set(true);
                    open.set(false);
                    refresh.update(|v| *v = v.wrapping_add(1));
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let code = a.code.clone();
    let lead = match &contested {
        None => view! {
            <p>
                {format!("Device '{name}' enrolled{from} with the code")}
                <b class="confirm-code">{code}</b> "."
            </p>
            <p>
                <b>"Does the device show this code?"</b>
                {replaces
                    .then_some(" Its current key keeps working until you confirm.")}
            </p>
        }
        .into_any(),
        Some(again) => view! {
            <p class="notice warn">
                {format!(
                    "Device '{name}' enrolled{from}, and its pairing code came back from {again} with another key. If your device did not pair, delete it."
                )}
            </p>
            <p>
                <b>"Type the code the device itself shows"</b>
                ", not one from this screen:"
            </p>
        }
        .into_any(),
    };
    view! {
        {lead}
        <div class="row" style="margin-top:6px">
            <ConfirmAction id=id awaiting=a refresh=refresh/>
            <Show
                when=move || deleting.get()
                fallback=move || {
                    view! {
                        <button class="btn danger sm" on:click=move |_| deleting.set(true)>
                            "Delete the device…"
                        </button>
                    }
                }
            >
                <span class="dim">"Delete it and its policy? "</span>
                <button class="btn danger sm" disabled=move || busy.get() on:click=delete>
                    "Delete the device"
                </button>
                <button class="btn ghost sm" on:click=move |_| deleting.set(false)>
                    "Keep it"
                </button>
            </Show>
        </div>
    }
}
