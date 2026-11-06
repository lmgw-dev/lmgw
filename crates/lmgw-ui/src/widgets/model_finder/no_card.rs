//! D9: with nothing to measure against (no GPU telemetry and no declared
//! budget), the card asks for the graphics card's memory. The answer is
//! saved after a confirm as the GPU memory budget Settings → GPU shows, and
//! the search runs again so the marks follow from it. Getting the model
//! stays possible without an answer.

use leptos::prelude::*;
use lmgw_api_types::MessageAck;
use serde_json::json;

use super::{words, Finder};

#[component]
pub(super) fn AskMemory(f: Finder) -> impl IntoView {
    let gib = RwSignal::new(String::new());
    let asking = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let err = RwSignal::new(None::<String>);
    let parsed = Memo::new(move |_| {
        gib.with(|g| g.trim().replace(',', ".").parse::<f64>().ok())
            .and_then(words::budget_mib)
    });
    let save = move |_| {
        let Some(mib) = parsed.get_untracked() else {
            return;
        };
        saving.set(true);
        err.set(None);
        f.scope.spawn(async move {
            let res = crate::api::post::<MessageAck, _>(
                "/api/op/settings_set_full",
                &json!({ "vram": { "budget_mb": mib } }),
            )
            .await;
            saving.set(false);
            match res {
                Ok(_) => {
                    asking.set(false);
                    // The marks follow from it: the same search again.
                    f.search(f.asked.get_untracked());
                }
                Err(e) => err.set(Some(e.to_string())),
            }
        });
    };
    view! {
        <div class="mf-ask">
            <p class="mf-line">
                "lmgw can't measure your graphics card yet, so it can't tell whether this model fits. You can still get it."
            </p>
            <div class="row mf-ask-row">
                <label class="mf-ask-label" for="mf-vram">
                    "How much memory does your graphics card have? "
                    <span class="dim">"· VRAM"</span>
                </label>
                <input
                    id="mf-vram"
                    class="input mf-ask-gb"
                    inputmode="decimal"
                    placeholder="8"
                    prop:value=move || gib.get()
                    on:input=move |ev| {
                        gib.set(event_target_value(&ev));
                        asking.set(false);
                    }
                />
                <span class="dim" title="In GiB, the unit lmgw says every size in: the number on the card's box">"GiB"</span>
                <Show
                    when=move || asking.get()
                    fallback=move || view! {
                        <button
                            class="btn sm"
                            disabled=move || parsed.get().is_none()
                            on:click=move |_| asking.set(true)
                        >
                            "Use this"
                        </button>
                    }
                >
                    <span class="mf-ask-confirm">
                        {move || format!(
                            "Save {} as the graphics card's memory in Settings → GPU? lmgw then measures every model against it.",
                            crate::fmt::fmt_bytes(parsed.get().unwrap_or(0) * 1024 * 1024),
                        )}
                    </span>
                    <button class="btn primary sm" disabled=move || saving.get() on:click=save>
                        {move || if saving.get() { "Saving…" } else { "Save" }}
                    </button>
                    <button class="btn ghost sm" on:click=move |_| asking.set(false)>"Cancel"</button>
                </Show>
            </div>
            {move || err.get().map(|e| view! { <p class="mf-line mf-warn">{e}</p> })}
        </div>
    }
}
