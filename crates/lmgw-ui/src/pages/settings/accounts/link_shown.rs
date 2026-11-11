//! A minted set-password link on the Accounts card, with Copy, until
//! dismissed (headless server design §5.5).

use leptos::prelude::*;
use lmgw_api_types::AccountLink;

use crate::widgets::use_toasts;

/// "valid until <local time>" from the link's expiry; the gateway's own
/// sentence (UTC) when the expiry does not read.
fn validity(l: &AccountLink) -> String {
    if l.expires_at.is_empty() {
        return l.validity.clone();
    }
    let local = super::when(&l.expires_at);
    if local == l.expires_at {
        l.validity.clone()
    } else {
        format!("valid until {local}")
    }
}

/// A minted link on screen, with Copy, until dismissed.
#[component]
pub(super) fn LinkShown(link: RwSignal<Option<AccountLink>>) -> impl IntoView {
    let toasts = use_toasts();
    move || {
        link.get().map(|l| {
            let url = l.url.clone();
            view! {
                <div class="notice" style="margin-top:8px">
                    <div>{format!("Hand this link to '{}' ({}):", l.login, validity(&l))}</div>
                    <code class="mono-sm" style="word-break:break-all">{l.url.clone()}</code>
                    <div class="row" style="gap:8px;margin-top:6px">
                        <button
                            class="btn sm"
                            on:click=move |_| {
                                crate::widgets::copy_secret(&url, toasts, "link copied".into())
                            }
                        >
                            "Copy"
                        </button>
                        <button class="btn ghost sm" on:click=move |_| link.set(None)>
                            "Done"
                        </button>
                    </div>
                </div>
            }
        })
    }
}
