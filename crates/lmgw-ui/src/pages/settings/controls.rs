//! Settings — the controls that act at once: GPU hold, apply, check now, theme and scale.

use super::*;

// ---------------------------------------------------------------------------
// Controls that apply at once
// ---------------------------------------------------------------------------

/// The manual GPU hold (gpu-hold design §3.1, §6): a state line and one
/// button. `active` is never part of the draft — only `ops::hold_set` may
/// flip it, since engaging it stops containers (§5) — the same op the
/// titlebar pill and the tray call.
#[component]
pub(super) fn HoldSwitch(page: Page) -> impl IntoView {
    let toasts = use_toasts();
    let live = use_live();
    // Seeded from the settings snapshot, then the live bus wins — a toggle
    // from the tray, MCP or another tab shows up here unprompted.
    let active = Memo::new(move |_| {
        live.vram
            .get()
            .map(|v| v.hold_active)
            .unwrap_or_else(|| page.server_flag("/hold/active"))
    });
    let busy = RwSignal::new(false);
    let toggle = move |_| crate::model_ops::hold_set(toasts, busy, !active.get_untracked());
    view! {
        <span class="set-line-label">"GPU hold"</span>
        <span class="chip" class:warn=move || active.get() class:off=move || !active.get()>
            <span class="dot"></span>
            {move || {
                if active.get() {
                    "engaged — local models are paused"
                } else {
                    "off — local models start on demand"
                }
            }}
        </span>
        <button
            class="btn sm"
            disabled=move || busy.get()
            title=move || {
                if active.get() {
                    "Let local models start again"
                } else {
                    "Stop every local model and refuse or reroute local requests until released"
                }
            }
            on:click=toggle
        >
            {move || match (busy.get(), active.get()) {
                (true, _) => "Working…",
                (false, true) => "Release hold",
                (false, false) => "Engage hold",
            }}
        </button>
        <span class="applies-now">"applies now"</span>
    }
}

/// Recreate every *running* model of a class with its freshly rendered argv
/// (`ops::container` group `apply`) — the follow-up step after a class
/// settings save when a running model was inheriting the value that just
/// changed. A no-op (and says so) when nothing of the class is up.
///
/// It recreates from the *saved* definition, so while this class's section
/// has unsaved edits it waits for Save: applying then would restart every
/// model of the class on the old values (review code:S5).
#[component]
pub(super) fn ClassApplyButton(
    target: &'static str,
    #[prop(into)] unsaved: Signal<bool>,
) -> impl IntoView {
    let ops = use_ops();
    let toasts = use_toasts();
    let busy = move || ops.busy(&class_key(target));
    let run = move |_| {
        let key = class_key(target);
        if !ops.start(&key) {
            return;
        }
        spawn_local(async move {
            let res = crate::api::post::<lmgw_api_types::ContainerAnswer, _>(
                "/api/op/container",
                &json!({ "target": target, "action": "apply" }),
            )
            .await;
            ops.finish(&key);
            match res {
                Ok(v) => toasts.ok(v.message().unwrap_or("applied").to_string()),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <button
            class="btn sm"
            disabled=move || busy() || unsaved.get()
            title=move || {
                if unsaved.get() {
                    "Save first: this recreates the running models from the saved settings, not the edits above"
                } else {
                    "Recreate every running model of this class with the saved settings above"
                }
            }
            on:click=run
        >
            {move || if busy() { "Applying…" } else { "Apply to running containers" }}
        </button>
    }
}

#[component]
pub(super) fn CheckNowButton() -> impl IntoView {
    let toasts = use_toasts();
    let checking = RwSignal::new(false);
    let check = move |_| {
        if checking.get_untracked() {
            return;
        }
        checking.set(true);
        spawn_local(async move {
            let res = crate::api::post::<MessageAck, _>("/api/op/update_check", &json!({})).await;
            checking.set(false);
            match res {
                Ok(v) => toasts.ok(v.message),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <button class="btn sm" disabled=move || checking.get() on:click=check>
            {move || if checking.get() { "Checking…" } else { "Check for app update now" }}
        </button>
    }
}

/// Theme: this window's own, stored where index.html reads it before paint.
#[component]
pub(super) fn ThemeField(id: String, hidden: Signal<bool>) -> impl IntoView {
    let theme = RwSignal::new(
        window()
            .local_storage()
            .ok()
            .flatten()
            .and_then(|s| s.get_item("lmgw-theme").ok().flatten())
            .unwrap_or_else(|| "dark".into()),
    );
    Effect::new(move |_| {
        let t = theme.get();
        let _ = document()
            .document_element()
            .map(|el| el.set_attribute("data-theme", &t));
        if let Ok(Some(storage)) = window().local_storage() {
            let _ = storage.set_item("lmgw-theme", &t);
        }
    });
    let opts = Signal::derive(|| {
        [("dark", "dark"), ("light", "light")]
            .into_iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect::<Vec<_>>()
    });
    view! {
        <Field label="Theme" unit="applies now" id=id hidden=hidden>
            <Select value=theme options=opts/>
        </Field>
    }
}

/// Interface scale. The value lives in the shared signal (the hotkeys write
/// it too), the select only mirrors it — hence the two effects instead of a
/// plain binding: one carries a Ctrl+= into the dropdown, the other carries
/// a pick back out. Both are guarded on "actually different", so they do not
/// chase each other.
#[component]
pub(super) fn ScaleField(id: String, hidden: Signal<bool>) -> impl IntoView {
    let scale = crate::ui_scale::use_ui_scale();
    let in_shell = crate::ui_scale::supported();
    let sel = RwSignal::new(scale.pct.get_untracked().to_string());
    Effect::new(move |_| {
        let pct = scale.pct.get().to_string();
        if sel.get_untracked() != pct {
            sel.set(pct);
        }
    });
    Effect::new(move |_| {
        if let Ok(pct) = sel.get().parse::<i32>() {
            if pct != scale.pct.get_untracked() {
                scale.set(pct);
            }
        }
    });
    let opts = Signal::derive(|| {
        crate::ui_scale::steps()
            .iter()
            .map(|p| (p.to_string(), format!("{p}%")))
            .collect::<Vec<_>>()
    });
    view! {
        <Field label="Interface scale" unit="applies now" id=id hidden=hidden>
            {if in_shell {
                view! { <Select value=sel options=opts/> }.into_any()
            } else {
                view! { <div class="dim set-static">"browser zoom · Ctrl +/-"</div> }.into_any()
            }}
        </Field>
    }
}
