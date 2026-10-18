//! The aux model editor: embedding, rerank and decision rows
//! (decision-models design §3.9). The kind decides what is shown: pooling
//! only for an embedder, the name under the decision prefix for a decision
//! row. The five row fields (`aux_fields`) and "Fill from plan"
//! (`aux_plan`) live in their own modules.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{public_names, AuxModel, AuxModelDetail, AuxModelSaved, SettingsFull};

use super::aux_fields::{self, AuxFields, CpuGpuArgsNote, CpuHoldNote, EngineFields};
use super::aux_plan::{self, Filling, PlanCard, PlanTargets, ShownPlan};
use super::aux_save::{save_body, takes_no_pooling, Draft};
use super::{hold_body, hold_signals, run_args_note, HoldFallback};
use crate::model_ops::ContainerStatusRow;
use crate::widgets::{use_toasts, ImageClass, ImagePicker, Modal, ModalFooter, ModalSize, Select};

#[component]
pub fn AuxEditor(
    editing: RwSignal<Option<AuxModel>>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let open = RwSignal::new(false);
    // Guarded both ways: an unconditional set would notify even when the
    // value is unchanged and the two effects would ping-pong forever.
    Effect::new(move |_| {
        let want = editing.get().is_some();
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && editing.get_untracked().is_some() {
            editing.set(None);
        }
    });
    view! {
        <Modal open=open title="Aux model (embedding / rerank / decision)" size=ModalSize::Wide guard=true>
            {move || {
                editing
                    .get()
                    .map(|m| view! { <AuxForm m=m open=open on_saved=on_saved/> }.into_any())
            }}
        </Modal>
    }
}

/// The models a row of `kind` may fall back to while the GPU is held: the
/// same kind, which the gateway checks too for a decision row.
fn fallback_tasks(kind: &str) -> &'static [&'static str] {
    match kind {
        "decision" => &["decision"],
        "rerank" => &["rerank"],
        _ => &["embedding"],
    }
}

#[component]
fn AuxForm(
    m: AuxModel,
    open: RwSignal<bool>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let create = m.id == 0;
    let id = m.id;
    let model_id = RwSignal::new(m.model_id.clone());
    let gguf_path = RwSignal::new(m.gguf_path.clone());
    // The plan compares a saved row on the file it was saved with.
    let saved_gguf_path = StoredValue::new(m.gguf_path.clone());
    let kind = RwSignal::new(m.kind.clone());
    let pooling = RwSignal::new(m.pooling.clone().unwrap_or_default());
    let ctx_size = RwSignal::new(m.ctx_size.map(|v| v.to_string()).unwrap_or_default());
    let fields = AuxFields::new(&m);
    let idle = RwSignal::new(m.idle_seconds.to_string());
    let args = RwSignal::new(m.args.join("\n"));
    // Per-model container overrides (per-model-containers §3.1) — empty
    // inherits the aux class settings (Settings → Runtimes → Aux).
    let image = RwSignal::new(m.image.clone().unwrap_or_default());
    let extra_run_args = RwSignal::new(
        m.extra_run_args
            .clone()
            .map(|a| a.join("\n"))
            .unwrap_or_default(),
    );
    let warm_start = RwSignal::new(m.warm_start);
    let (hold_mode, hold_alias) = hold_signals(&m.hold_fallback_mode, &m.hold_fallback);
    let saved_model_id = RwSignal::new(if create {
        String::new()
    } else {
        m.model_id.clone()
    });
    let class_settings =
        LocalResource::new(|| crate::api::get::<SettingsFull>("/api/settings-full"));
    let settings = Signal::derive(move || class_settings.get().and_then(|r| r.ok()));
    // The server's view of the saved row: what it finds wrong with it (a
    // projector that is not a file, a flag the kind refuses).
    let detail = LocalResource::new(move || {
        let mid = saved_model_id.get();
        async move {
            if mid.trim().is_empty() {
                return None;
            }
            crate::api::get::<AuxModelDetail>(format!(
                "/api/local-model?target=aux&model_id={}",
                String::from(js_sys::encode_uri_component(&mid))
            ))
            .await
            .ok()
        }
    });
    let saving = RwSignal::new(false);
    let planning = RwSignal::new(false);
    let plan = RwSignal::new(None::<ShownPlan>);
    let plan_targets = PlanTargets {
        model_id,
        kind,
        pooling,
        ctx_size,
        fields,
    };
    // The control disappears for a kind that takes no pooling rather than
    // offering a value the server will reject.
    let takes_no_pooling = move || kind.with(|k| takes_no_pooling(k));
    let kind_opts = Signal::derive(|| {
        [
            ("embed", "embedding"),
            ("rerank", "rerank"),
            ("decision", "decision"),
        ]
        .into_iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect::<Vec<_>>()
    });
    let pooling_opts = Signal::derive(|| {
        [
            ("", "model default"),
            ("none", "none"),
            ("mean", "mean"),
            ("cls", "cls"),
            ("last", "last"),
            ("rank", "rank"),
        ]
        .into_iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect::<Vec<_>>()
    });
    // The name clients will call the row by, as drafted.
    let public_name = move || {
        let mid = model_id.with(|m| m.trim().to_string());
        if mid.is_empty() {
            return None;
        }
        settings.with(|s| {
            s.as_ref()
                .map(|s| aux_fields::public_name(&kind.get(), &mid, s))
        })
    };

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        let body = hold_body(hold_mode, hold_alias).and_then(|(mode, alias)| {
            save_body(&Draft {
                id: (!create).then_some(id),
                model_id: model_id.get_untracked(),
                gguf_path: gguf_path.get_untracked(),
                kind: kind.get_untracked(),
                pooling: pooling.get_untracked(),
                ctx_size: ctx_size.get_untracked(),
                idle_seconds: idle.get_untracked(),
                args: args.get_untracked(),
                image: image.get_untracked(),
                extra_run_args: extra_run_args.get_untracked(),
                warm_start: warm_start.get_untracked(),
                hold_fallback_mode: mode,
                hold_fallback: alias,
                fields: fields.draft(),
            })
        });
        let body = match body {
            Ok(b) => b,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<AuxModelSaved, _>("/api/op/aux_model_set", &body).await;
            saving.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v.message);
                    open.set(false);
                    on_saved();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    view! {
        {move || {
            let problems = detail.get().flatten().map(|d| d.problems).unwrap_or_default();
            (!problems.is_empty())
                .then(|| {
                    view! {
                        <section class="card edit-section problems editor-top">
                            <h3>"Problems"</h3>
                            {problems
                                .iter()
                                .map(|p| view! { <div class="problem">{p.clone()}</div> })
                                .collect_view()}
                        </section>
                    }
                })
        }}
        <div class="card-flow editor-flow">
            <section class="card edit-section">
                <h3>"Model"</h3>
                <div class="field-grid">
                    <div class="field">
                        <label>"Model id"</label>
                        <input class="input mono"
                            prop:value=move || model_id.get()
                            on:input=move |ev| model_id.set(event_target_value(&ev))/>
                    </div>
                    <div class="field">
                        <label>"Served as"</label>
                        <Select value=kind options=kind_opts/>
                    </div>
                </div>
                {move || {
                    public_name()
                        .map(|name| {
                            view! {
                                <p class="dim mini-note">
                                    "Clients call it " <span class="mono-sm">{name}</span> " on "
                                    <span class="mono-sm">{public_names::aux_endpoint(&kind.get())}</span> "."
                                </p>
                            }
                        })
                }}
                <div class="field-grid">
                    <div class="field wide">
                        <label>"GGUF path" <span class="field-unit">"relative to the aux models dir"</span></label>
                        <input class="input mono"
                            prop:value=move || gguf_path.get()
                            on:input=move |ev| gguf_path.set(event_target_value(&ev))/>
                    </div>
                    <div class="field wide">
                        <label>"Projector" <span class="field-unit">"relative to the aux models dir · blank = none"</span></label>
                        <input class="input mono"
                            prop:value=move || fields.mmproj_path.get()
                            on:input=move |ev| fields.mmproj_path.set(event_target_value(&ev))/>
                    </div>
                    <Show when=move || !takes_no_pooling()>
                        <div class="field">
                            <label>"Pooling"</label>
                            <Select value=pooling options=pooling_opts/>
                        </div>
                    </Show>
                    <div class="field">
                        <label>"Context size"</label>
                        <input class="input mono" prop:value=move || ctx_size.get()
                            on:input=move |ev| ctx_size.set(event_target_value(&ev))/>
                    </div>
                    <div class="field">
                        <label>"Idle seconds" <span class="field-unit">"0 = never sleep"</span></label>
                        <input class="input mono" prop:value=move || idle.get()
                            on:input=move |ev| idle.set(event_target_value(&ev))/>
                    </div>
                </div>
                <div class="row" style="margin-top:8px; gap:8px">
                    <button
                        class="btn sm"
                        disabled=move || planning.get()
                        title="Read the GGUF's header and fill the kind, pooling, context, batch sizes, projector and (for a new row) where it runs; nothing is saved"
                        on:click=move |_| {
                            let path = gguf_path.get_untracked();
                            let filling = if create {
                                Filling::New
                            } else {
                                Filling::Saved {
                                    id,
                                    same_file: saved_gguf_path
                                        .with_value(|s| s.trim() == path.trim()),
                                }
                            };
                            aux_plan::fill_from_plan(
                                path,
                                filling,
                                plan_targets,
                                plan,
                                planning,
                                toasts,
                            )
                        }
                    >
                        {move || if planning.get() { "Planning…" } else { "Fill from plan" }}
                    </button>
                </div>
                <div class="field">
                    <label>"Extra llama-server flags (one argument per line)"</label>
                    <textarea class="input mono ta" prop:value=move || args.get()
                        on:input=move |ev| args.set(event_target_value(&ev))></textarea>
                </div>
            </section>
            <section class="card edit-section">
                <h3>"Engine"</h3>
                <EngineFields f=fields args=args settings=settings/>
            </section>
            <PlanCard plan=plan/>
            <section class="card edit-section">
                <h3>"Container"</h3>
                <label class="row dim" style="gap:5px">
                    <input
                        type="checkbox"
                        prop:checked=move || warm_start.get()
                        on:change=move |ev| warm_start.set(event_target_checked(&ev))
                    />
                    "start at boot (warm start)"
                </label>
                <div class="field">
                    <label>"Hold fallback"</label>
                    {move || {
                        let tasks = fallback_tasks(&kind.get());
                        view! {
                            <HoldFallback
                                mode=hold_mode
                                alias=hold_alias
                                inherit_label="Inherit · none"
                                tasks=tasks
                            />
                        }
                    }}
                </div>
                <p class="dim mini-note">
                    "Where this model's requests go while a GPU hold (Settings → GPU → Hold) is "
                    "active, to a model of the same kind. Aux models never inherit the global "
                    "fallback — a different embedder silently corrupts a vector index — so "
                    "\"Inherit\" here means the same as \"None\": refuse with a 503."
                </p>
                <CpuHoldNote f=fields/>
                <div class="field">
                    <label>"Image override"</label>
                    <ImagePicker
                        value=image
                        class=ImageClass::Aux
                        clearable=true
                        placeholder=Signal::derive(move || {
                            settings
                                .with(|s| s.as_ref().map(|s| s.aux_router.image.clone()))
                                .unwrap_or_default()
                        })
                    />
                </div>
                <div class="field">
                    <label>"Extra podman run args override (one argument per line)"</label>
                    <textarea
                        class="input mono ta"
                        placeholder=move || {
                            settings
                                .with(|s| {
                                    s.as_ref()
                                        .map(|s| aux_fields::inherited_run_args(fields.on_cpu(), s))
                                })
                                .unwrap_or_default()
                        }
                        prop:value=move || extra_run_args.get()
                        on:input=move |ev| extra_run_args.set(event_target_value(&ev))
                    ></textarea>
                </div>
                <CpuGpuArgsNote f=fields own=extra_run_args/>
                <p class="dim mini-note">
                    "Blank image/args inherit the aux class settings (Settings → Runtimes → Aux)." " " {run_args_note::OVERRIDE_REPLACES}
                </p>
                <ContainerStatusRow class="aux" model_id=saved_model_id/>
            </section>
        </div>
        <ModalFooter>
            <button class="btn ghost" on:click=move |_| open.set(false)>
                "Cancel"
            </button>
            <button class="btn primary" disabled=move || saving.get() on:click=save>
                {move || if saving.get() { "Saving…" } else { "Save model" }}
            </button>
        </ModalFooter>
    }
}

#[cfg(test)]
mod tests {
    use super::fallback_tasks;

    #[test]
    fn a_fallback_is_picked_among_models_of_the_row_s_kind() {
        assert_eq!(fallback_tasks("decision"), ["decision"]);
        assert_eq!(fallback_tasks("rerank"), ["rerank"]);
        assert_eq!(fallback_tasks("embed"), ["embedding"]);
    }
}
