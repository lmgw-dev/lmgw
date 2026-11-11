//! Settings — forgetting auto headroom's learned growth (per-model
//! containers §4, "Headroom"): a game run without the hold, say, taught a
//! growth the desktop never has. Confirm, then do.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::HeadroomNow;
use serde_json::json;

use super::Page;
use crate::widgets::use_toasts;

/// The learned growth, the button and its confirmation. The answer is the
/// headroom as it is now, which the sentence above reads.
#[component]
pub(super) fn ForgetGrowth(page: Page, id: String, hidden: Signal<bool>) -> impl IntoView {
    let toasts = use_toasts();
    let confirming = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let learned = move || {
        page.data.with(|d| {
            d.as_ref()
                .and_then(|v| v.pointer("/headroom_now/learned_bytes"))
                .and_then(|v| v.as_u64())
        })
    };
    let forget = move |_| {
        busy.set(true);
        spawn_local(async move {
            let res =
                crate::api::post::<HeadroomNow, _>("/api/op/headroom_forget", &json!({})).await;
            busy.set(false);
            confirming.set(false);
            match res {
                Ok(now) => {
                    toasts.ok("Learned growth forgotten".to_string());
                    page.data.update(|d| {
                        if let Some(d) = d.as_mut() {
                            d["headroom_now"] = json!(now);
                        }
                    });
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <div class="set-line" id=id hidden=move || hidden.get()>
            <Show
                when=move || confirming.get()
                fallback=move || {
                    view! {
                        <button
                            class="btn"
                            disabled=move || learned().is_none()
                            on:click=move |_| confirming.set(true)
                        >
                            "Forget learned growth…"
                        </button>
                    }
                }
            >
                <div class="notice warn">
                    "Every reading of other programs' GPU memory is deleted. Auto headroom uses \
                     the fixed value until it has watched the card for a day again."
                </div>
                <div class="row" style="gap:8px;margin-top:8px">
                    <button class="btn primary" disabled=move || busy.get() on:click=forget>
                        "Forget"
                    </button>
                    <button class="btn ghost" on:click=move |_| confirming.set(false)>
                        "Cancel"
                    </button>
                </div>
            </Show>
        </div>
    }
}
