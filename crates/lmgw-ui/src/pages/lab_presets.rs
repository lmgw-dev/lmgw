//! A lab's presets in its form column (labs-store design §3.2, §5): named
//! form parts kept on the server through `/labs/api/{lab}/presets`. One card
//! for any lab and kind: pick a saved one and Load it into the form, Update
//! it with the form as it is now (from the version read: a preset that moved
//! since is the server's 409, said as a toast), Delete it, or save the form
//! under a new name. The Image lab keeps prompts (`image_prompt`), the Audio
//! lab its speech settings (`audio_settings`).

use leptos::prelude::*;
use lmgw_api_types::lab_store::{
    LabKind, LabPresetBody, LabPresetKind, LabPresetList, LabPresetUpdate, LabPresetView,
    LabPresetWrite,
};

use crate::api;
use crate::scope::Scope;
use crate::widgets::{use_toasts, ConfirmButton, Select};

/// `/labs/api/{lab}/presets`.
fn base(lab: LabKind) -> String {
    format!("/labs/api/{}/presets", lab.as_str())
}

/// The select's options: each preset by id, labelled by name.
fn options(presets: &[LabPresetView]) -> Vec<(String, String)> {
    presets
        .iter()
        .map(|p| (p.id.to_string(), p.name.clone()))
        .collect()
}

/// The presets card. `current` reads the form part as it is now; `on_load`
/// puts a preset's body into the form.
#[component]
pub(super) fn PresetsCard(
    lab: LabKind,
    kind: LabPresetKind,
    /// The card's heading: "Prompt presets", "Speech presets".
    title: &'static str,
    current: Callback<(), LabPresetBody>,
    on_load: Callback<LabPresetBody>,
) -> impl IntoView {
    let scope = Scope::new();
    let toasts = use_toasts();
    let presets = RwSignal::new(Vec::<LabPresetView>::new());
    let pick = RwSignal::new(String::new());
    let name = RwSignal::new(String::new());
    let busy = RwSignal::new(false);

    let reload = move || {
        let href = format!("{}?kind={}", base(lab), kind.as_str());
        scope.spawn(async move {
            match api::get::<LabPresetList>(href).await {
                Ok(l) => presets.set(l.presets),
                Err(e) => toasts.err(format!("the presets did not load: {e}")),
            }
        });
    };
    reload();

    let chosen = move || {
        let id = pick.get();
        presets.with(|v| v.iter().find(|p| p.id.to_string() == id).cloned())
    };
    let load = move |_| {
        if let Some(p) = chosen() {
            on_load.run(p.preset);
            toasts.ok(format!("loaded '{}'", p.name));
        }
    };
    let save = move |_| {
        // One save at a time: Enter in the name field does not ask the
        // button whether it is disabled.
        if busy.get_untracked() {
            return;
        }
        let n = name.get_untracked().trim().to_string();
        if n.is_empty() {
            toasts.err("give the preset a name first");
            return;
        }
        let body = LabPresetWrite {
            name: n,
            preset: current.run(()),
        };
        busy.set(true);
        scope.spawn(async move {
            match api::post::<LabPresetView, _>(base(lab), &body).await {
                Ok(p) => {
                    toasts.ok(format!("saved '{}'", p.name));
                    let id = p.id.to_string();
                    presets.update(|v| {
                        v.push(p);
                        v.sort_by(|a, b| a.name.cmp(&b.name));
                    });
                    pick.set(id);
                    name.set(String::new());
                }
                Err(e) => toasts.err(format!("saving failed: {e}")),
            }
            busy.set(false);
        });
    };
    let update = move |_| {
        let Some(p) = chosen() else { return };
        let body = LabPresetUpdate {
            name: p.name.clone(),
            preset: current.run(()),
            updated_at: p.updated_at.clone(),
        };
        busy.set(true);
        scope.spawn(async move {
            match api::post::<LabPresetView, _>(format!("{}/{}", base(lab), p.id), &body).await {
                Ok(fresh) => {
                    toasts.ok(format!("updated '{}'", fresh.name));
                    presets.update(|v| {
                        if let Some(slot) = v.iter_mut().find(|x| x.id == fresh.id) {
                            *slot = fresh;
                        }
                    });
                }
                Err(e) => {
                    toasts.err(format!("updating failed: {e}"));
                    reload();
                }
            }
            busy.set(false);
        });
    };
    let delete = Callback::new(move |()| {
        let Some(p) = chosen() else { return };
        scope.spawn(async move {
            match api::post::<LabPresetList, _>(format!("{}/{}/delete", base(lab), p.id), &()).await
            {
                Ok(l) => {
                    presets.set(
                        l.presets
                            .into_iter()
                            .filter(|x| x.preset.kind() == kind)
                            .collect(),
                    );
                    pick.set(String::new());
                    toasts.ok(format!("deleted '{}'", p.name));
                }
                Err(e) => toasts.err(format!("deleting failed: {e}")),
            }
        });
    });
    let opts = Signal::derive(move || presets.with(|v| options(v)));
    let none_picked = move || chosen().is_none();

    view! {
        <div class="card lp-card">
            <h3 class="lab-h3">{title}</h3>
            <div class="lp-row">
                <span class="lp-select">
                    <Select value=pick options=opts placeholder="pick a preset"/>
                </span>
                <span class="lp-actions">
                <button
                    type="button"
                    class="btn ghost sm lp-load"
                    disabled=none_picked
                    title="Put this preset into the form"
                    on:click=load
                >
                    "Load"
                </button>
                <button
                    type="button"
                    class="btn ghost sm lp-update"
                    disabled=move || none_picked() || busy.get()
                    title="Overwrite this preset with the form as it is now"
                    on:click=update
                >
                    "Update"
                </button>
                <ConfirmButton
                    label="Delete"
                    confirm=Signal::derive(move || {
                        chosen().map(|p| format!("Delete '{}'?", p.name)).unwrap_or_default()
                    })
                    disabled=Signal::derive(none_picked)
                    title="Delete this preset on the server"
                    on_confirm=delete
                />
                </span>
            </div>
            <div class="lp-row">
                <input
                    class="input lp-name"
                    placeholder="a name for the form as it is"
                    prop:value=move || name.get()
                    on:input=move |ev| name.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            save(());
                        }
                    }
                />
                <button
                    type="button"
                    class="btn ghost sm lp-save"
                    disabled=move || busy.get() || name.with(|n| n.trim().is_empty())
                    on:click=move |_| save(())
                >
                    "Save as preset"
                </button>
            </div>
            <Show when=move || presets.with(Vec::is_empty)>
                <p class="dim mini-note">"None saved yet: name the form as it is and save it."</p>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preset_is_offered_by_id_and_named_by_its_name() {
        let p = LabPresetView {
            id: 7,
            lab: LabKind::Image,
            name: "night".into(),
            preset: LabPresetBody::ImagePrompt(Box::default()),
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert_eq!(options(&[p]), [("7".to_string(), "night".to_string())]);
        assert_eq!(base(LabKind::Audio), "/labs/api/audio/presets");
    }
}
