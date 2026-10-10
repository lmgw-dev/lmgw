//! Edit modals for cloud aliases, embedding, audio and image models. One modal
//! per kind at page level (never per row); the form component is rebuilt each
//! time the editing signal changes, so state is always fresh.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{
    AliasView, AudioModel, AuxModel, GgufFiles, ImageModelDetail, Params, ReasoningControl,
    SettingsFull, UpstreamModelEntry, UpstreamModelsResponse, UpstreamsResponse,
};
use serde_json::{json, Map, Value};

use crate::catalog::CatalogEntry;
use crate::model_ops::ContainerStatusRow;

mod audio_placement;
mod run_args_note;
use crate::widgets::model_picker::ListStatus;
use crate::widgets::{
    use_toasts, ImageClass, ImagePicker, MenuItem, Modal, ModalFooter, ModalSize, ModelPicker,
    RowMenu, Select,
};

fn num_field<T: std::str::FromStr>(label: &str, raw: &str) -> Result<Option<T>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<T>()
        .map(Some)
        .map_err(|_| format!("{label}: '{raw}' is not a number"))
}

/// The three ways a model can answer while the GPU hold is on (gpu-hold
/// design §2/§3.2), as the mode picker offers them. `inherit` is labelled by
/// the caller: only chat rows inherit the global fallback — aux, audio and
/// image rows never do, so for them it reads as "none" rather than promising
/// a fallback that never applies.
fn hold_modes(inherit_label: &'static str) -> Signal<Vec<(String, String)>> {
    Signal::derive(move || {
        vec![
            ("inherit".to_string(), inherit_label.to_string()),
            ("none".to_string(), "None — refuse with a 503".to_string()),
            ("alias".to_string(), "Route to another model".to_string()),
        ]
    })
}

/// A model's GPU-hold fallback: the mode, and — when it routes elsewhere —
/// which model, picked from the gateway catalog. Local models are listed but
/// not pickable: the hold is exactly when this GPU is not available to them.
/// `tasks` narrows the picker to models that can stand in (an embedding row
/// falls back to an embedding model, not a chat one).
#[component]
pub(super) fn HoldFallback(
    mode: RwSignal<String>,
    alias: RwSignal<String>,
    inherit_label: &'static str,
    tasks: &'static [&'static str],
) -> impl IntoView {
    view! {
        <div class="hold-pick">
            <Select value=mode options=hold_modes(inherit_label)/>
            <Show when=move || mode.get() == "alias">
                <ModelPicker
                    value=alias
                    tasks=tasks
                    recent_key="hold"
                    disallow=(
                        Callback::new(|e: CatalogEntry| e.local),
                        "the fallback must not need this GPU",
                    )
                />
            </Show>
        </div>
    }
}

/// A stored `hold_fallback_mode`/`hold_fallback` pair as the two signals
/// [`HoldFallback`] edits. `pub(super)`: the candidate-alias editor
/// (`candidate_alias_editor.rs`) builds its own fallback picker instead of
/// [`HoldFallback`] (its `disallow` also needs to exclude other candidate
/// aliases, which that component has no parameter for), but the mode/value
/// convention — and the leniency of what counts as a valid stored mode — is
/// exactly this one.
pub(super) fn hold_signals(
    mode: &str,
    alias: &Option<String>,
) -> (RwSignal<String>, RwSignal<String>) {
    let mode = match mode {
        "alias" | "none" => mode,
        _ => "inherit",
    };
    (
        RwSignal::new(mode.to_string()),
        RwSignal::new(alias.clone().unwrap_or_default()),
    )
}

/// The save body's `(hold_fallback_mode, hold_fallback)`, or why it cannot
/// be saved: routing to another model needs that model. `pub(super)`: see
/// [`hold_signals`]'s doc comment.
pub(super) fn hold_body(
    mode: RwSignal<String>,
    alias: RwSignal<String>,
) -> Result<(String, Option<String>), String> {
    let m = mode.get_untracked();
    if m != "alias" {
        return Ok((m, None));
    }
    let a = alias.get_untracked().trim().to_string();
    if a.is_empty() {
        return Err("hold fallback: pick the model to route to, or choose another mode".into());
    }
    Ok((m, Some(a)))
}

// ---------------------------------------------------------------------------
// Alias editor
// ---------------------------------------------------------------------------

/// `editing`: Some(view) opens the form; a default `AliasView` (id 0) means
/// "create new".
#[component]
pub fn AliasEditor(
    editing: RwSignal<Option<AliasView>>,
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
        <Modal open=open title="Alias" size=ModalSize::Wide guard=true>
            {move || {
                editing
                    .get()
                    .map(|a| view! { <AliasForm a=a open=open on_saved=on_saved/> }.into_any())
            }}
        </Modal>
    }
}

#[component]
fn AliasForm(
    a: AliasView,
    open: RwSignal<bool>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let create = a.id == 0;
    let id = a.id;
    let alias = RwSignal::new(a.alias.clone());
    let upstream_id = RwSignal::new(if a.upstream_id > 0 {
        a.upstream_id.to_string()
    } else {
        String::new()
    });
    let upstream_model = RwSignal::new(a.upstream_model_id.clone());
    // Owner override of the derived /v1/models capability facts
    // (model-capabilities design §7) — same "JSON object as text, empty
    // clears" convention as `chat_template_kwargs` on the local model form.
    let capabilities_override = RwSignal::new(
        a.capabilities_override
            .as_ref()
            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
            .unwrap_or_default(),
    );
    let o = &a.param_overrides;
    let temperature = RwSignal::new(o.temperature.map(|v| v.to_string()).unwrap_or_default());
    let top_p = RwSignal::new(o.top_p.map(|v| v.to_string()).unwrap_or_default());
    let top_k = RwSignal::new(o.top_k.map(|v| v.to_string()).unwrap_or_default());
    let max_tokens = RwSignal::new(o.max_tokens.map(|v| v.to_string()).unwrap_or_default());
    let seed = RwSignal::new(o.seed.map(|v| v.to_string()).unwrap_or_default());
    let stop = RwSignal::new(o.stop.join(", "));
    // Per-request reasoning control default (model-capabilities design §5.1):
    // an alias-level `on`/`off`/effort/budget that a client's own
    // headers/body fields override. "" on the enabled select means "say
    // nothing, leave the route's own default alone" — distinct from an
    // explicit "off".
    let reasoning_enabled = RwSignal::new(
        match o.reasoning.as_ref().and_then(|r| r.enabled) {
            Some(true) => "on",
            Some(false) => "off",
            None => "",
        }
        .to_string(),
    );
    let reasoning_effort = RwSignal::new(
        o.reasoning
            .as_ref()
            .and_then(|r| r.effort.clone())
            .unwrap_or_default(),
    );
    let reasoning_budget = RwSignal::new(
        o.reasoning
            .as_ref()
            .and_then(|r| r.budget_tokens)
            .map(|v| v.to_string())
            .unwrap_or_default(),
    );
    let reasoning_enabled_opts = Signal::derive(|| {
        [("", "leave default"), ("on", "on"), ("off", "off")]
            .into_iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect::<Vec<_>>()
    });
    let saving = RwSignal::new(false);

    let upstreams = LocalResource::new(|| crate::api::get::<UpstreamsResponse>("/api/upstreams"));
    let upstream_opts = Signal::derive(move || {
        upstreams
            .get()
            .and_then(|r| r.ok())
            .map(|u| {
                u.upstreams
                    .into_iter()
                    .map(|u| (u.id.to_string(), u.name))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let upstream_name = move || {
        let uid = upstream_id.get();
        upstream_opts
            .get()
            .into_iter()
            .find(|(id, _)| *id == uid)
            .map(|(_, name)| name)
            .unwrap_or_else(|| format!("upstream #{uid}"))
    };

    // The picked upstream's live catalog, for the model picker. A failed
    // fetch says so under the picker (the id can still be typed); it is
    // never an empty list that looks like "this upstream has no models".
    let raw = RwSignal::new(Vec::<UpstreamModelEntry>::new());
    let status = RwSignal::new(ListStatus::Ready);
    let generation = StoredValue::new(0u64);
    let fetch_catalog = move || {
        let uid = upstream_id.get_untracked();
        let gen = generation.get_value() + 1;
        generation.set_value(gen);
        raw.set(Vec::new());
        if uid.is_empty() {
            status.set(ListStatus::Ready);
            return;
        }
        status.set(ListStatus::Loading);
        spawn_local(async move {
            let res =
                crate::api::get::<UpstreamModelsResponse>(format!("/api/upstream-models?id={uid}"))
                    .await;
            if generation.try_get_value() != Some(gen) {
                return;
            }
            match res {
                Ok(r) => {
                    // An older gateway sends ids only; they still list.
                    let mut list = if r.entries.is_empty() {
                        r.models
                            .into_iter()
                            .map(|id| UpstreamModelEntry {
                                id,
                                ..Default::default()
                            })
                            .collect()
                    } else {
                        r.entries
                    };
                    list.sort_by(|a, b| a.id.cmp(&b.id));
                    raw.set(list);
                    status.set(ListStatus::Ready);
                }
                Err(e) => status.set(ListStatus::Failed(e.to_string())),
            }
        });
    };
    Effect::new(move |_| {
        upstream_id.track();
        fetch_catalog();
    });
    let entries = Memo::new(move |_| {
        let name = upstream_name();
        raw.with(|r| {
            r.iter()
                .map(|e| CatalogEntry::from_upstream(&name, e))
                .collect::<Vec<_>>()
        })
    });

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        let overrides = (|| -> Result<Params, String> {
            let reasoning_enabled_v = match reasoning_enabled.get_untracked().as_str() {
                "on" => Some(true),
                "off" => Some(false),
                _ => None,
            };
            let reasoning_effort_v = {
                let e = reasoning_effort.get_untracked();
                let e = e.trim();
                (!e.is_empty()).then(|| e.to_string())
            };
            let reasoning_budget_v =
                num_field("reasoning budget", &reasoning_budget.get_untracked())?;
            let reasoning = if reasoning_enabled_v.is_none()
                && reasoning_effort_v.is_none()
                && reasoning_budget_v.is_none()
            {
                None
            } else {
                Some(ReasoningControl {
                    enabled: reasoning_enabled_v,
                    effort: reasoning_effort_v,
                    budget_tokens: reasoning_budget_v,
                })
            };
            Ok(Params {
                temperature: num_field("temperature", &temperature.get_untracked())?,
                top_p: num_field("top-p", &top_p.get_untracked())?,
                top_k: num_field("top-k", &top_k.get_untracked())?,
                max_tokens: num_field("max tokens", &max_tokens.get_untracked())?,
                presence_penalty: None,
                frequency_penalty: None,
                seed: num_field("seed", &seed.get_untracked())?,
                reasoning,
                stop: stop
                    .get_untracked()
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect(),
            })
        })();
        let overrides = match overrides {
            Ok(o) => o,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let uid: i64 = match upstream_id.get_untracked().parse() {
            Ok(v) => v,
            Err(_) => {
                toasts.err("pick an upstream");
                return;
            }
        };
        let args = json!({
            "action": if create { "create" } else { "update" },
            "id": if create { Value::Null } else { json!(id) },
            "alias": alias.get_untracked().trim(),
            "upstream_id": uid,
            "upstream_model": upstream_model.get_untracked().trim(),
            "overrides": overrides,
            // A blank textarea is sent as an empty string, which the server
            // (`ops::parse_capabilities_override`) reads as "clear" — same
            // convention as `chat_template_kwargs` on the local model form.
            "capabilities_override": capabilities_override.get_untracked().trim(),
        });
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/alias_set", &args).await;
            saving.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("saved")
                        .to_string());
                    open.set(false);
                    on_saved();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let text = |label: &'static str, sig: RwSignal<String>| {
        view! {
            <div class="field">
                <label>{label}</label>
                <input
                    class="input mono"
                    prop:value=move || sig.get()
                    on:input=move |ev| sig.set(event_target_value(&ev))
                />
            </div>
        }
    };

    view! {
        <div class="card-flow editor-flow">
            <section class="card edit-section">
                <h3>"Route"</h3>
                <div class="field-grid">
                    <div class="field">
                        <label>"Alias" <span class="field-unit">"the name clients request"</span></label>
                        <input
                            class="input mono"
                            prop:value=move || alias.get()
                            on:input=move |ev| alias.set(event_target_value(&ev))
                        />
                    </div>
                    <div class="field">
                        <label>"Upstream"</label>
                        <Select value=upstream_id options=upstream_opts placeholder="pick an upstream"/>
                    </div>
                    <div class="field wide">
                        <label>"Upstream model id" <span class="field-unit">"what the upstream calls it"</span></label>
                        <ModelPicker
                            value=upstream_model
                            entries=Signal::from(entries)
                            status=Signal::from(status)
                            allow_custom=true
                            disabled=Signal::derive(move || upstream_id.with(String::is_empty))
                        />
                        {move || match status.get() {
                            ListStatus::Failed(e) => {
                                Some(
                                    view! {
                                        <div class="field-err" role="alert">
                                            {format!("{} catalog failed: {e} · ", upstream_name())}
                                            <button type="button" class="link-btn" on:click=move |_| fetch_catalog()>
                                                "Retry"
                                            </button>
                                            " · or type the id in the picker"
                                        </div>
                                    },
                                )
                            }
                            _ => None,
                        }}
                        <div class="field-hint">
                            {move || {
                                if upstream_id.with(String::is_empty) {
                                    return "Pick the upstream first; its catalog fills the list.".to_string();
                                }
                                match status.get() {
                                    ListStatus::Loading => format!("Loading {}'s catalog…", upstream_name()),
                                    ListStatus::Failed(_) => String::new(),
                                    ListStatus::Ready => {
                                        format!(
                                            "{} lists {} models — pick one, or type an id it does not list.",
                                            upstream_name(),
                                            crate::fmt::grouped(entries.with(Vec::len) as u64),
                                        )
                                    }
                                }
                            }}
                        </div>
                    </div>
                </div>
            </section>
            <section class="card edit-section">
                <h3>"Parameter overrides"</h3>
                <div class="field-grid">
                    {text("Temperature", temperature)}
                    {text("Top-p", top_p)}
                    {text("Top-k", top_k)}
                    {text("Max tokens", max_tokens)}
                    {text("Seed", seed)}
                    {text("Stop sequences (comma-separated)", stop)}
                    <div class="field">
                        <label>"Reasoning"</label>
                        <Select value=reasoning_enabled options=reasoning_enabled_opts placeholder="leave default"/>
                    </div>
                    {text("Reasoning effort", reasoning_effort)}
                    {text("Reasoning budget (tokens)", reasoning_budget)}
                </div>
                <p class="field-hint">"Blank leaves the upstream's default. A request's own fields win over these."</p>
            </section>
            <section class="card edit-section">
                <h3>"Capability override"</h3>
                <div class="field">
                    <label>"JSON object"</label>
                    <textarea
                        class="input mono ta"
                        prop:value=move || capabilities_override.get()
                        on:input=move |ev| capabilities_override.set(event_target_value(&ev))
                    ></textarea>
                </div>
                <p class="dim mini-note">
                    "Merged over what lmgw derives for /v1/models — optional keys capabilities "
                    "(deep-merged), max_output_tokens, notes (appended). Use it only for facts the "
                    "catalog does not state; the result is published with source \"owner\". Blank "
                    "clears."
                </p>
            </section>
        </div>
        <ModalFooter>
            <button class="btn ghost" on:click=move |_| open.set(false)>
                "Cancel"
            </button>
            <button class="btn primary" disabled=move || saving.get() on:click=save>
                {move || if saving.get() { "Saving…" } else { "Save alias" }}
            </button>
        </ModalFooter>
    }
}

// ---------------------------------------------------------------------------
// Aux model editor (embeddings + rerank)
// ---------------------------------------------------------------------------

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
        <Modal open=open title="Aux model (embedding / rerank)" size=ModalSize::Wide guard=true>
            {move || {
                editing
                    .get()
                    .map(|m| view! { <AuxForm m=m open=open on_saved=on_saved/> }.into_any())
            }}
        </Modal>
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
    let kind = RwSignal::new(m.kind.clone());
    let pooling = RwSignal::new(m.pooling.clone().unwrap_or_default());
    let ctx_size = RwSignal::new(m.ctx_size.map(|v| v.to_string()).unwrap_or_default());
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
    let saving = RwSignal::new(false);
    // A reranker takes no pooling at all — `reranking = true` selects rank
    // pooling itself and a second key breaks it — so the control disappears
    // rather than offering a value the server will reject.
    let is_rerank = move || kind.get() == "rerank";
    let kind_opts = Signal::derive(|| {
        [("embed", "embedding"), ("rerank", "rerank")]
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

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        let ctx: Option<i64> = match num_field("context size", &ctx_size.get_untracked()) {
            Ok(v) => v,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let idle_v: i64 = match idle.get_untracked().trim().parse() {
            Ok(v) => v,
            Err(_) => {
                toasts.err("idle seconds must be an integer");
                return;
            }
        };
        let arg_lines: Vec<String> = args
            .get_untracked()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        let era_lines: Vec<String> = extra_run_args
            .get_untracked()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        let era_empty = era_lines.is_empty();
        let (hf_mode, hf_alias) = match hold_body(hold_mode, hold_alias) {
            Ok(h) => h,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let body = json!({
            "action": if create { "create" } else { "update" },
            "id": if create { Value::Null } else { json!(id) },
            "model_id": model_id.get_untracked().trim(),
            "gguf_path": gguf_path.get_untracked().trim(),
            "kind": kind.get_untracked(),
            "pooling": if is_rerank() { String::new() } else { pooling.get_untracked() },
            "ctx_size": ctx,
            "hold_fallback_mode": hf_mode,
            "hold_fallback": hf_alias,
            "args": arg_lines,
            "idle_seconds": idle_v,
            "image": image.get_untracked().trim(),
            // Empty inherits the class settings, whose run args carry the GPU
            // and SELinux flags: no list is sent, and the field is named in
            // `clear` so an override saved before goes too.
            "extra_run_args": if era_empty { Value::Null } else { json!(era_lines) },
            "warm_start": warm_start.get_untracked(),
            "clear": if era_empty { "extra_run_args" } else { "" },
        });
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/aux_model_set", &body).await;
            saving.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("saved")
                        .to_string());
                    open.set(false);
                    on_saved();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    view! {
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
                    <div class="field wide">
                        <label>"GGUF path" <span class="field-unit">"relative to the aux models dir"</span></label>
                        <input class="input mono"
                            prop:value=move || gguf_path.get()
                            on:input=move |ev| gguf_path.set(event_target_value(&ev))/>
                    </div>
                    <Show when=move || !is_rerank()>
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
                <div class="field">
                    <label>"Extra llama-server flags (one per line)"</label>
                    <textarea class="input mono ta" prop:value=move || args.get()
                        on:input=move |ev| args.set(event_target_value(&ev))></textarea>
                </div>
            </section>
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
                    <HoldFallback
                        mode=hold_mode
                        alias=hold_alias
                        inherit_label="Inherit · none"
                        tasks=&["embedding", "rerank"]
                    />
                </div>
                <p class="dim mini-note">
                    "Where this model's requests go while a GPU hold (Settings → GPU → Hold) is "
                    "active. Embedding/rerank models never inherit the global fallback — a "
                    "different one silently corrupts a vector index — so \"Inherit\" here means "
                    "the same as \"None\": refuse with a 503."
                </p>
                <div class="field">
                    <label>"Image override"</label>
                    <ImagePicker
                        value=image
                        class=ImageClass::Aux
                        clearable=true
                        placeholder=Signal::derive(move || {
                            class_settings
                                .get()
                                .and_then(|r| r.ok())
                                .map(|s| s.aux_router.image)
                                .unwrap_or_default()
                        })
                    />
                </div>
                <div class="field">
                    <label>"Extra podman run args override (one per line)"</label>
                    <textarea
                        class="input mono ta"
                        placeholder=move || {
                            class_settings
                                .get()
                                .and_then(|r| r.ok())
                                .map(|s| s.aux_router.extra_run_args.join("\n"))
                                .unwrap_or_default()
                        }
                        prop:value=move || extra_run_args.get()
                        on:input=move |ev| extra_run_args.set(event_target_value(&ev))
                    ></textarea>
                </div>
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

// ---------------------------------------------------------------------------
// Audio model editor
// ---------------------------------------------------------------------------

/// audio.cpp server task names (CLI `--task` values) — mirrors
/// `lmgw_core::web::audio::AUDIO_TASKS`. Not shared across the crate
/// boundary (that list is `pub(crate)` to lmgw-core), so kept in sync by
/// hand; the server is the actual source of truth and rejects anything
/// outside this set regardless of what the dropdown offers.
const AUDIO_TASKS: [&str; 14] = [
    "tts", "asr", "gen", "clon", "vc", "svc", "s2s", "sep", "vad", "diar", "align", "vdes", "spk",
    "midi",
];

/// Parse a JSON-object textarea into a map; blank text is an empty map
/// (clears the field), matching the "always send the form's full state"
/// convention the rest of this file uses.
fn json_obj_field(label: &str, raw: &str) -> Result<serde_json::Map<String, Value>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(format!("{label}: must be a JSON object")),
        Err(e) => Err(format!("{label}: invalid JSON — {e}")),
    }
}

fn pretty_json_obj(m: &serde_json::Map<String, Value>) -> String {
    if m.is_empty() {
        String::new()
    } else {
        serde_json::to_string_pretty(m).unwrap_or_default()
    }
}

/// `default_voice_preset` is a bare preset name or an inline JSON object
/// (`AudioPatch` doc comment in lmgw-core/src/web/api.rs); render either back
/// into a single text field.
fn default_voice_preset_text(v: &Option<Value>) -> String {
    match v {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

/// Parses the same field back out: blank clears (`Some("")`, per the
/// `AudioPatch::default_voice_preset` convention — this form always supplies
/// the field, so "keep current" (`None`) never applies here), `{...}` is an
/// inline preset object, anything else is a bare preset name.
fn default_voice_preset_field(raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Value::String(String::new()));
    }
    if raw.starts_with('{') {
        match serde_json::from_str::<Value>(raw) {
            Ok(v @ Value::Object(_)) => Ok(v),
            Ok(_) => Err("default voice preset: inline value must be a JSON object".into()),
            Err(e) => Err(format!("default voice preset: invalid JSON — {e}")),
        }
    } else {
        Ok(Value::String(raw.to_string()))
    }
}

#[component]
pub fn AudioEditor(
    editing: RwSignal<Option<AudioModel>>,
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
        <Modal open=open title="Audio model" size=ModalSize::Wide guard=true>
            {move || {
                editing
                    .get()
                    .map(|m| view! { <AudioForm m=m open=open on_saved=on_saved/> }.into_any())
            }}
        </Modal>
    }
}

#[component]
fn AudioForm(
    m: AudioModel,
    open: RwSignal<bool>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let create = m.id == 0;
    let id = m.id;
    let model_id = RwSignal::new(m.model_id.clone());
    let family = RwSignal::new(m.family.clone());
    let path = RwSignal::new(m.path.clone());
    let task = RwSignal::new(m.task.clone());
    let mode = RwSignal::new(m.mode.clone());
    let load_options = RwSignal::new(pretty_json_obj(&m.load_options));
    let session_options = RwSignal::new(pretty_json_obj(&m.session_options));
    let request_defaults = RwSignal::new(pretty_json_obj(&m.default_request_options));
    // Per-model engine knobs; blank/"" inherits the audio class settings.
    let lazy = RwSignal::new(match m.lazy {
        None => String::new(),
        Some(true) => "true".to_string(),
        Some(false) => "false".to_string(),
    });
    let busy_timeout = RwSignal::new(m.busy_timeout_ms.map(|v| v.to_string()).unwrap_or_default());
    let placement = audio_placement::RowPlacement::new(&m);
    let spec_override = RwSignal::new(m.model_spec_override.clone().unwrap_or_default());
    let config_id = RwSignal::new(m.config_id.clone().unwrap_or_default());
    let weight_id = RwSignal::new(m.weight_id.clone().unwrap_or_default());
    let voice_presets = RwSignal::new(pretty_json_obj(&m.voice_presets));
    let default_voice_preset = RwSignal::new(default_voice_preset_text(&m.default_voice_preset));
    // Per-model container overrides (per-model-containers §3.1) — empty
    // inherits the audio class settings (Settings → Runtimes → Audio).
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
    let saving = RwSignal::new(false);
    let task_opts = Signal::derive(|| {
        AUDIO_TASKS
            .iter()
            .map(|t| (t.to_string(), t.to_string()))
            .collect::<Vec<_>>()
    });
    let mode_opts = Signal::derive(|| {
        [("offline", "offline"), ("streaming", "streaming")]
            .into_iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect::<Vec<_>>()
    });
    let lazy_opts = Signal::derive(|| {
        [
            ("", "Inherit the class"),
            ("true", "lazy · load on first request"),
            ("false", "eager · load at container start"),
        ]
        .into_iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect::<Vec<_>>()
    });

    // The cached spec catalog, for one thing: the options *this family*
    // declares. Load and session options are two free-form JSON boxes below,
    // and until audio.cpp published a typed schema the only way to learn what
    // may go in them was to read the family's C++ source.
    let catalog = LocalResource::new(|| async {
        crate::api::get::<lmgw_api_types::AudioCatalog>("/api/audio/catalog")
            .await
            .ok()
    });
    let family_options = move || {
        let want = family.get();
        let want = want.trim();
        if want.is_empty() {
            return None;
        }
        catalog
            .get()
            .flatten()?
            .families
            .into_iter()
            .find(|f| f.family == want)
            .map(|f| f.options)
            .filter(|o| !(o.load.is_empty() && o.session.is_empty() && o.request.is_empty()))
    };

    // Stored reference clips, offered as copy-paste hints for voice_ref
    // presets — the same directory `/audio-lab` uploads into. Hidden
    // entirely if the endpoint errors or the dir isn't configured.
    let refs = LocalResource::new(|| async {
        crate::api::get::<lmgw_api_types::audio_lab::ClipList>("/audio-lab/api/refs")
            .await
            .ok()
    });

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        let load_opts = match json_obj_field("load options", &load_options.get_untracked()) {
            Ok(v) => v,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let session_opts = match json_obj_field("session options", &session_options.get_untracked())
        {
            Ok(v) => v,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let request_opts =
            match json_obj_field("request defaults", &request_defaults.get_untracked()) {
                Ok(v) => v,
                Err(e) => {
                    toasts.err(e);
                    return;
                }
            };
        let presets = match json_obj_field("voice presets", &voice_presets.get_untracked()) {
            Ok(v) => v,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let default_preset = match default_voice_preset_field(&default_voice_preset.get_untracked())
        {
            Ok(v) => v,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let era_lines: Vec<String> = extra_run_args
            .get_untracked()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        let era_empty = era_lines.is_empty();
        // Blank means "inherit the class", which is a *reset* rather than a
        // value — the patch says so by name, the way extra_run_args does.
        let lazy_raw = lazy.get_untracked();
        let busy_raw = busy_timeout.get_untracked().trim().to_string();
        let busy_ms = if busy_raw.is_empty() {
            None
        } else {
            match busy_raw.parse::<i64>() {
                Ok(v) if v >= 0 => Some(v),
                _ => {
                    toasts.err("busy timeout: whole milliseconds, 0 or more (blank inherits)");
                    return;
                }
            }
        };
        let (backend, threads, mut clear) = match placement.body() {
            Ok(b) => b,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        if era_empty {
            clear.push("extra_run_args");
        }
        if lazy_raw.is_empty() {
            clear.push("lazy");
        }
        if busy_ms.is_none() {
            clear.push("busy_timeout_ms");
        }
        let (hf_mode, hf_alias) = match hold_body(hold_mode, hold_alias) {
            Ok(h) => h,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let body = json!({
            "action": if create { "create" } else { "update" },
            "id": if create { Value::Null } else { json!(id) },
            "model_id": model_id.get_untracked().trim(),
            "family": family.get_untracked().trim(),
            "path": path.get_untracked().trim(),
            "task": task.get_untracked(),
            "hold_fallback_mode": hf_mode,
            "hold_fallback": hf_alias,
            "mode": mode.get_untracked(),
            "load_options": load_opts,
            "session_options": session_opts,
            "default_request_options": request_opts,
            "lazy": match lazy_raw.as_str() {
                "true" => json!(true),
                "false" => json!(false),
                _ => Value::Null,
            },
            "busy_timeout_ms": match busy_ms {
                Some(v) => json!(v),
                None => Value::Null,
            },
            "backend": backend,
            "threads": threads,
            "model_spec_override": spec_override.get_untracked().trim(),
            "config_id": config_id.get_untracked().trim(),
            "weight_id": weight_id.get_untracked().trim(),
            "voice_presets": presets,
            "default_voice_preset": default_preset,
            "image": image.get_untracked().trim(),
            // Blank inherits the class (named in `clear` above): no list.
            "extra_run_args": if era_empty { Value::Null } else { json!(era_lines) },
            "warm_start": warm_start.get_untracked(),
            "clear": clear.join(" "),
        });
        saving.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/audio_model_set", &body).await;
            saving.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("saved")
                        .to_string());
                    open.set(false);
                    on_saved();
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    view! {
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
                        <label>"Family"</label>
                        <input class="input mono"
                            prop:value=move || family.get()
                            on:input=move |ev| family.set(event_target_value(&ev))/>
                    </div>
                    <div class="field wide">
                        <label>"Path" <span class="field-unit">"relative to the audio models dir"</span></label>
                        <input class="input mono"
                            prop:value=move || path.get()
                            on:input=move |ev| path.set(event_target_value(&ev))/>
                    </div>
                    <div class="field">
                        <label>"Task"</label>
                        <Select value=task options=task_opts/>
                    </div>
                    <div class="field">
                        <label>"Mode"</label>
                        <Select value=mode options=mode_opts/>
                    </div>
                </div>
            </section>
            <section class="card edit-section">
                <h3>"Options"</h3>
                <div class="field">
                    <label>"Load options (JSON object)"</label>
                    <textarea class="input mono ta" prop:value=move || load_options.get()
                        on:input=move |ev| load_options.set(event_target_value(&ev))></textarea>
                </div>
                <div class="field">
                    <label>"Session options (JSON object)"</label>
                    <textarea class="input mono ta" prop:value=move || session_options.get()
                        on:input=move |ev| session_options.set(event_target_value(&ev))></textarea>
                </div>
                {move || {
                    family_options()
                        .map(|options| {
                            view! { <super::audio_catalog::FamilyOptions options=options/> }
                        })
                }}
                <div class="field">
                    <label>"Request defaults (JSON object, applied to every call)"</label>
                    <textarea class="input mono ta" prop:value=move || request_defaults.get()
                        on:input=move |ev| request_defaults.set(event_target_value(&ev))></textarea>
                </div>
                <p class="dim mini-note">
                    "Load and session options configure the model; request defaults are defaults for "
                    "the call — a language, a speaking rate, a step count — and any request that names "
                    "one wins over them."
                </p>
            </section>
            <section class="card edit-section">
                <h3>"Engine"</h3>
                <div class="field-grid">
                    <div class="field">
                        <label>"Load timing"</label>
                        <Select value=lazy options=lazy_opts/>
                    </div>
                    <div class="field">
                        <label>"Busy timeout" <span class="field-unit">"ms · blank inherits, 0 = wait forever"</span></label>
                        <input class="input mono"
                            prop:value=move || busy_timeout.get()
                            on:input=move |ev| busy_timeout.set(event_target_value(&ev))/>
                    </div>
                    <audio_placement::RunsOnFields p=placement settings=settings/>
                </div>
                <audio_placement::RunsOnNote/>
                <p class="dim mini-note">
                    "Busy timeout bounds how long a request waits for this model while it is already "
                    "running, and caps what a request may ask for. Minutes-long work (music "
                    "generation) needs more than the class default; a short TTS row can have less."
                </p>
                <div class="field-grid">
                    <div class="field wide">
                        <label>"Model spec override" <span class="field-unit">"path under the models dir"</span></label>
                        <input class="input mono"
                            prop:value=move || spec_override.get()
                            on:input=move |ev| spec_override.set(event_target_value(&ev))/>
                    </div>
                    <div class="field">
                        <label>"Config id" <span class="field-unit">"blank = the loader picks"</span></label>
                        <input class="input mono"
                            prop:value=move || config_id.get()
                            on:input=move |ev| config_id.set(event_target_value(&ev))/>
                    </div>
                    <div class="field">
                        <label>"Weight id" <span class="field-unit">"blank = the loader picks"</span></label>
                        <input class="input mono"
                            prop:value=move || weight_id.get()
                            on:input=move |ev| weight_id.set(event_target_value(&ev))/>
                    </div>
                </div>
                <p class="dim mini-note">
                    "A spec override is a <family>.json (or a directory of them) that replaces the "
                    "container image's own catalog for this row — which is how a family newer than "
                    "the image gets served. Config/weight ids pick one asset when the model directory "
                    "holds several."
                </p>
            </section>
            <section class="card edit-section">
                <h3>"Voices"</h3>
                <div class="field">
                    <label>"Voice presets (JSON object: name → { voice_id | voice_ref, … })"</label>
                    <textarea class="input mono ta" prop:value=move || voice_presets.get()
                        on:input=move |ev| voice_presets.set(event_target_value(&ev))></textarea>
                    {move || {
                        let clips = refs.get().flatten().map(|r| r.clips).unwrap_or_default();
                        (!clips.is_empty())
                            .then(|| {
                                view! {
                                    <div class="dim mini-note">
                                        {format!(
                                            "{} stored voice clips (container path — paste into a voice_ref):",
                                            clips.len(),
                                        )}
                                    </div>
                                    <For each=move || clips.clone() key=|c| c.server_path.clone() let:c>
                                        <div class="copy-line">
                                            <span class="mono-sm">{c.server_path.clone()}</span>
                                            <span class="dim">
                                                {format!(" ({})", crate::fmt::human_bytes(c.size))}
                                            </span>
                                            <CopyBtn text=c.server_path.clone()/>
                                        </div>
                                    </For>
                                }
                            })
                    }}
                </div>
                <div class="field">
                    <label>"Default voice preset" <span class="field-unit">"a preset name, or an inline JSON object; empty clears"</span></label>
                    <input class="input mono"
                        prop:value=move || default_voice_preset.get()
                        on:input=move |ev| default_voice_preset.set(event_target_value(&ev))/>
                </div>
            </section>
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
                    <HoldFallback
                        mode=hold_mode
                        alias=hold_alias
                        inherit_label="Inherit · none"
                        tasks=&["tts", "asr"]
                    />
                </div>
                <p class="dim mini-note">
                    "Where this model's requests go while a GPU hold (Settings → GPU → Hold) is active. "
                    "Audio models never inherit the global fallback, only their own row does, so "
                    "\"Inherit\" here means the same as \"None\": refuse with a 503."
                </p>
                <audio_placement::CpuHoldNote p=placement settings=settings/>
                <div class="field">
                    <label>"Image override"</label>
                    <ImagePicker
                        value=image
                        class=ImageClass::Audio
                        clearable=true
                        placeholder=Signal::derive(move || {
                            class_settings
                                .get()
                                .and_then(|r| r.ok())
                                .map(|s| s.audio.image)
                                .unwrap_or_default()
                        })
                        backend=Signal::derive(move || {
                            class_settings
                                .get()
                                .and_then(|r| r.ok())
                                .map(|s| s.audio.backend)
                                .unwrap_or_default()
                        })
                    />
                </div>
                <div class="field">
                    <label>"Extra podman run args override (one per line)"</label>
                    <textarea
                        class="input mono ta"
                        placeholder=move || {
                            settings
                                .get()
                                .map(|s| audio_placement::inherited_args(placement, &s))
                                .unwrap_or_default()
                        }
                        prop:value=move || extra_run_args.get()
                        on:input=move |ev| extra_run_args.set(event_target_value(&ev))
                    ></textarea>
                </div>
                <audio_placement::CpuGpuArgsNote p=placement own=extra_run_args settings=settings/>
                <p class="dim mini-note">
                    "Blank image/args inherit the audio class settings (Settings → Runtimes → Audio)." " " {run_args_note::OVERRIDE_REPLACES}
                </p>
                <ContainerStatusRow class="audio" model_id=saved_model_id/>
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

/// Copy-to-clipboard button — small local twin of `overview::CopyBtn` (that
/// one is private to its module, so this file gets its own rather than
/// exporting across pages for a 10-line widget).
#[component]
fn CopyBtn(text: String) -> impl IntoView {
    let copied = RwSignal::new(false);
    view! {
        <button
            class="copy-btn"
            title="Copy"
            on:click=move |_| {
                let _ = window().navigator().clipboard().write_text(&text);
                copied.set(true);
                set_timeout(move || copied.set(false), std::time::Duration::from_secs(2));
            }
        >
            {move || if copied.get() { "✓" } else { "⧉" }}
        </button>
    }
}

// ---------------------------------------------------------------------------
// Image model editor (image-generation design §8)
// ---------------------------------------------------------------------------

/// The `files` roles the form gives a picker of its own, in the order a
/// pipeline is read: what loads it, then what hangs off it, then the two
/// directories lmgw renders unconditionally.
///
/// `model` and `diffusion_model` are **not** here — they are exclusive
/// alternatives and share one radio-driven row above this list. Anything
/// sd-server's help knows and this list does not is reachable through the
/// "other role" rows, so a flag a new build adds needs no lmgw release.
///
/// The third element is the `role_guess` the image file listing uses for this
/// role, which only sorts the picker: every file stays selectable, because the
/// guess is a filename heuristic and a pipeline is free to disagree with it.
const IMAGE_FILE_ROLES: &[(&str, &str, &str)] = &[
    ("vae", "VAE", "vae"),
    ("clip_l", "CLIP-L text encoder", "text_encoder"),
    ("clip_g", "CLIP-G text encoder", "text_encoder"),
    ("t5xxl", "T5-XXL text encoder", "text_encoder"),
    ("llm", "LLM text encoder (GGUF)", "text_encoder"),
    ("llm_vision", "LLM vision tower", "text_encoder"),
    ("taesd", "TAESD (tiny autoencoder)", "vae"),
    ("control_net", "ControlNet", "diffusion"),
    ("lora_model_dir", "LoRA directory", "lora"),
    ("hires_upscalers_dir", "Upscaler directory", "upscaler"),
];

/// `--type` quantization values — **examples, non-exhaustive**, exactly as
/// sd-server's own `--help` gives them:
///
/// ```text
/// --type    weight type (examples: f32, f16, q4_0, q4_1, q5_0, q5_1, q8_0,
///           q2_K, q3_K, q4_K). If not specified, the default is the type of
///           the weight file
/// ```
///
/// Hardcoded, and the one place in this page that is: the list lives in that
/// help, the parser that reads it is `lmgw_core::sdcpp_caps`, and that crate
/// is server-side only — nothing publishes the values over `/api`. Two things
/// this copy has to get right, because sd.cpp matches the string exactly: the
/// **case** (`q4_K`, not `q4_k` — the k-quant names are capitalized in
/// `sd_type_name`) and the fact that the list is examples. Anything it omits
/// — `bf16`, `q5_K`, `q6_K` — is still reachable through the raw args JSON
/// box, which merges last and wins over every widget.
const IMAGE_TYPES: &[&str] = &[
    "f32", "f16", "q4_0", "q4_1", "q5_0", "q5_1", "q8_0", "q2_K", "q3_K", "q4_K",
];

/// The `args` keys with a widget. `switch` keys render as a checkbox and are
/// stored as a bare `true` (which the argv renderer emits as a lone flag);
/// everything else is a text field parsed as JSON-ish on save, so `steps 8`
/// reaches the argv as a number and `sampling_method euler` as a string —
/// exactly what `ops::KeyMap` does with the text form.
struct ImageArg {
    key: &'static str,
    label: &'static str,
    hint: &'static str,
    switch: bool,
}

const IMAGE_ARGS: &[ImageArg] = &[
    ImageArg {
        key: "type",
        label: "Weight type",
        hint: "--type: quantize the loaded weights",
        switch: false,
    },
    ImageArg {
        key: "cfg_scale",
        label: "CFG scale",
        hint: "",
        switch: false,
    },
    ImageArg {
        key: "steps",
        label: "Sampling steps",
        hint: "",
        switch: false,
    },
    ImageArg {
        key: "sampling_method",
        label: "Sampler",
        hint: "the server's own list is on a running model",
        switch: false,
    },
    ImageArg {
        key: "scheduler",
        label: "Scheduler",
        hint: "",
        switch: false,
    },
    ImageArg {
        key: "width",
        label: "Default width",
        hint: "px",
        switch: false,
    },
    ImageArg {
        key: "height",
        label: "Default height",
        hint: "px",
        switch: false,
    },
    ImageArg {
        key: "seed",
        label: "Seed",
        hint: "-1 = random per request",
        switch: false,
    },
    ImageArg {
        key: "diffusion_fa",
        label: "Flash attention (diffusion)",
        hint: "",
        switch: true,
    },
    ImageArg {
        key: "vae_tiling",
        label: "VAE tiling",
        hint: "decode in tiles — less VRAM at high resolutions",
        switch: true,
    },
    ImageArg {
        key: "clip_on_cpu",
        label: "Text encoders on CPU",
        hint: "",
        switch: true,
    },
    ImageArg {
        key: "offload_to_cpu",
        label: "Offload weights to CPU",
        hint: "",
        switch: true,
    },
];

/// One stored `args` value as the text field shows it: a string without its
/// quotes, everything else as JSON.
fn image_arg_text(v: Option<&Value>) -> String {
    match v {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

/// The inverse: a number stays a number and a bool stays a bool (`steps = 8`
/// has to reach the argv as `8`, not `"8"`), everything else is the string it
/// is — the same rule `ops::KeyMap` applies to its text form.
fn image_arg_value(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ (Value::Number(_) | Value::Bool(_))) => v,
        _ => Value::from(raw),
    }
}

#[component]
pub fn ImageEditor(
    editing: RwSignal<Option<lmgw_api_types::ImageModel>>,
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
        <Modal open=open title="Image model" size=ModalSize::Wide guard=true>
            {move || {
                editing
                    .get()
                    .map(|m| view! { <ImageForm m=m open=open on_saved=on_saved/> }.into_any())
            }}
        </Modal>
    }
}

#[component]
fn ImageForm(
    m: lmgw_api_types::ImageModel,
    open: RwSignal<bool>,
    on_saved: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let toasts = use_toasts();
    let modal = crate::widgets::use_modal();
    let create = m.id == 0;
    let id = m.id;
    let model_id = RwSignal::new(m.model_id.clone());

    // --- files -------------------------------------------------------------
    // The exclusive pair first: which of the two loads this pipeline decides
    // which of the two inputs is live, and the other one stays disabled rather
    // than disappearing, so the alternative is visible.
    let entry_key = RwSignal::new(
        if m.files
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.trim().is_empty())
        {
            "model"
        } else {
            "diffusion_model"
        }
        .to_string(),
    );
    // One path signal per alternative rather than one shared one: a checkpoint
    // and a standalone diffusion model are different files, so flipping the
    // radio must not carry the other one's path across — and the inactive row
    // shows its own (usually empty) value instead of a lie.
    let checkpoint_path = RwSignal::new(
        m.files
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    );
    let diffusion_path = RwSignal::new(
        m.files
            .get("diffusion_model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    );
    let role_sigs: StoredValue<Vec<(&'static str, RwSignal<String>)>> = StoredValue::new(
        IMAGE_FILE_ROLES
            .iter()
            .map(|(key, _, _)| {
                (
                    *key,
                    RwSignal::new(
                        m.files
                            .get(*key)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ),
                )
            })
            .collect(),
    );
    // Every stored role the form has no widget for, kept as editable
    // key/value rows so a round-trip never drops one — and so a flag this
    // build knows and this form does not can still be set.
    let known: Vec<&str> = IMAGE_FILE_ROLES
        .iter()
        .map(|(k, _, _)| *k)
        .chain(["model", "diffusion_model"])
        .collect();
    let other_files = RwSignal::new(
        m.files
            .iter()
            .filter(|(k, _)| !known.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
            .collect::<Vec<_>>(),
    );
    // The roles on screen: the ones this row names, plus any added since the
    // form opened. A pipeline uses two or three of the ten, and ten empty
    // path rows buried the ones that matter; the rest are one pick away.
    let shown_roles = RwSignal::new(
        IMAGE_FILE_ROLES
            .iter()
            .map(|(key, _, _)| *key)
            .filter(|key| {
                m.files
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|v| !v.trim().is_empty())
            })
            .collect::<Vec<&'static str>>(),
    );
    // Rows added or taken off by a button fire no input event; the modal is
    // told directly, so Esc asks about them too (review code:C7).
    let touch_editor = move || {
        if let Some(m) = modal {
            m.touch();
        }
    };
    let add_role_items = Signal::derive(move || {
        let shown = shown_roles.get();
        let mut items: Vec<MenuItem> = IMAGE_FILE_ROLES
            .iter()
            .filter(|(key, _, _)| !shown.contains(key))
            .map(|(key, label, _)| {
                let key = *key;
                MenuItem::new(format!("{label} · {key}"), move || {
                    shown_roles.update(|s| s.push(key));
                    touch_editor();
                })
            })
            .collect();
        items.push(
            MenuItem::new("Another flag key…", move || {
                other_files.update(|rows| rows.push((String::new(), String::new())));
                touch_editor();
            })
            .title("Any other file flag this sd-server build knows, by its --help key"),
        );
        items
    });
    let add_role_label = Signal::derive(move || {
        let left = IMAGE_FILE_ROLES.len() - shown_roles.with(Vec::len);
        format!("+ add file role ({left} more)")
    });

    // --- args --------------------------------------------------------------
    let arg_sigs: StoredValue<Vec<(&'static str, RwSignal<String>)>> = StoredValue::new(
        IMAGE_ARGS
            .iter()
            .filter(|a| !a.switch)
            .map(|a| (a.key, RwSignal::new(image_arg_text(m.args.get(a.key)))))
            .collect(),
    );
    let switch_sigs: StoredValue<Vec<(&'static str, RwSignal<bool>)>> = StoredValue::new(
        IMAGE_ARGS
            .iter()
            .filter(|a| a.switch)
            .map(|a| {
                (
                    a.key,
                    RwSignal::new(m.args.get(a.key).and_then(Value::as_bool).unwrap_or(false)),
                )
            })
            .collect(),
    );
    // Whatever the widgets do not cover, verbatim, so a saved row round-trips
    // through this form unchanged. It merges *last* on save: a key typed here
    // wins over the widget above holding the same key.
    let widget_args: Vec<&str> = IMAGE_ARGS.iter().map(|a| a.key).collect();
    let raw_args = RwSignal::new(pretty_json_obj(
        &m.args
            .iter()
            .filter(|(k, _)| !widget_args.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    ));

    let img_gen = RwSignal::new(m.modes.is_empty() || m.modes.iter().any(|s| s == "img_gen"));
    let vid_gen = RwSignal::new(m.modes.iter().any(|s| s == "vid_gen"));
    let edit = RwSignal::new(m.edit);

    // --- the block every class editor has ----------------------------------
    let enabled = RwSignal::new(m.enabled);
    let idle = RwSignal::new(m.idle_seconds.to_string());
    let image = RwSignal::new(m.image.clone().unwrap_or_default());
    let extra_run_args = RwSignal::new(
        m.extra_run_args
            .clone()
            .map(|a| a.join("\n"))
            .unwrap_or_default(),
    );
    let warm_start = RwSignal::new(m.warm_start);
    let (hold_mode, hold_alias) = hold_signals(&m.hold_fallback_mode, &m.hold_fallback);
    let capabilities_override = RwSignal::new(
        m.capabilities_override
            .as_ref()
            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
            .unwrap_or_default(),
    );
    let saved_once = RwSignal::new(false);
    let saved_model_id = RwSignal::new(if create {
        String::new()
    } else {
        m.model_id.clone()
    });
    let class_settings =
        LocalResource::new(|| crate::api::get::<SettingsFull>("/api/settings-full"));
    let saving = RwSignal::new(false);

    // The server's view of this row: which files it can actually find, what
    // it would run, and what it thinks is wrong. Re-read after every save, so
    // the rendered command line below is never one edit stale.
    let detail_reload = RwSignal::new(0u32);
    let detail = LocalResource::new(move || {
        detail_reload.get();
        let mid = saved_model_id.get();
        async move {
            if mid.trim().is_empty() {
                return None;
            }
            crate::api::get::<ImageModelDetail>(format!(
                "/api/local-model?target=image&model_id={}",
                js_sys::encode_uri_component(&mid)
                    .as_string()
                    .unwrap_or(mid)
            ))
            .await
            .ok()
        }
    });
    // The save's own answer — `warnings[]` and the freshly rendered command
    // line. Shown beside the server's, because a create has no stored row to
    // read back until it lands.
    let save_warnings = RwSignal::new(Vec::<String>::new());
    let save_command_line = RwSignal::new(String::new());
    // Re-seed the extra-args textarea from the server's grouped rendering,
    // once per read (not on every detail change), so it is never rewritten
    // under a half-typed edit. Only this field: everything else the form
    // already holds in exactly the shape it was saved in.
    let era_applied = RwSignal::new(None::<u32>);
    Effect::new(move |_| {
        let generation = detail_reload.get();
        let Some(Some(d)) = detail.get() else { return };
        if era_applied.get_untracked() == Some(generation) {
            return;
        }
        era_applied.set(Some(generation));
        extra_run_args.set(d.extra_run_args.unwrap_or_default());
    });

    let collect_files = move || -> Result<Map<String, Value>, String> {
        let mut files = Map::new();
        // Only the picked alternative is stored: `image_model_set` refuses a
        // row that names both.
        let key = entry_key.get_untracked();
        let entry = match key.as_str() {
            "model" => checkpoint_path.get_untracked(),
            _ => diffusion_path.get_untracked(),
        };
        let entry = entry.trim().to_string();
        if !entry.is_empty() {
            files.insert(key, Value::from(entry));
        }
        role_sigs.with_value(|rs| {
            for (key, sig) in rs {
                let v = sig.get_untracked().trim().to_string();
                if !v.is_empty() {
                    files.insert((*key).to_string(), Value::from(v));
                }
            }
        });
        for (k, v) in other_files.get_untracked() {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.is_empty() && v.is_empty() {
                continue;
            }
            if k.is_empty() {
                return Err(format!("a files row has a path ('{v}') but no role key"));
            }
            files.insert(k, Value::from(v));
        }
        Ok(files)
    };

    let collect_args = move || -> Result<Map<String, Value>, String> {
        let mut args = Map::new();
        arg_sigs.with_value(|a| {
            for (key, sig) in a {
                let raw = sig.get_untracked();
                let raw = raw.trim();
                if !raw.is_empty() {
                    args.insert((*key).to_string(), image_arg_value(raw));
                }
            }
        });
        switch_sigs.with_value(|s| {
            for (key, sig) in s {
                // Only a set switch is stored: an absent key is sd-server's
                // own default, and `false` is not a value the argv renderer
                // has a spelling for.
                if sig.get_untracked() {
                    args.insert((*key).to_string(), Value::Bool(true));
                }
            }
        });
        for (k, v) in json_obj_field("raw args", &raw_args.get_untracked())? {
            args.insert(k, v);
        }
        Ok(args)
    };

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        let files = match collect_files() {
            Ok(f) => f,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let args = match collect_args() {
            Ok(a) => a,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        // An empty list is itself the clear for this field (`StrList::items`
        // of `[]` is empty, and a supplied value always replaces), so `modes`
        // never goes into `clear` — where its name would also be a substring
        // trap for the `contains` the patch reads `clear` with.
        let mut modes: Vec<&str> = Vec::new();
        if img_gen.get_untracked() {
            modes.push("img_gen");
        }
        if vid_gen.get_untracked() {
            modes.push("vid_gen");
        }
        // The textarea goes over as *text*, not as one token per line: the
        // patch's `ArgList` splits it with shell quoting (`parse_args_text`),
        // which is what makes `--device nvidia.com/gpu=all` two argv elements
        // instead of one 26-character flag. Same as the chat editor.
        let era = extra_run_args.get_untracked();
        let era_empty = era.trim().is_empty();
        let idle_v = match num_field::<i64>("idle seconds", &idle.get_untracked()) {
            Ok(v) => v.unwrap_or(0),
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        let (hf_mode, hf_alias) = match hold_body(hold_mode, hold_alias) {
            Ok(h) => h,
            Err(e) => {
                toasts.err(e);
                return;
            }
        };
        // Empty inherits the class settings; `image` clears on an empty
        // string, the rest have to be named — `extra_run_args` so an override
        // saved before goes too (blank text alone leaves it as it is), and an
        // empty `capabilities_override` string is not a clear for this patch.
        let mut clear: Vec<&str> = Vec::new();
        if era_empty {
            clear.push("extra_run_args");
        }
        if capabilities_override.get_untracked().trim().is_empty() {
            clear.push("capabilities_override");
        }
        let body = json!({
            "action": if create { "create" } else { "update" },
            "id": if create { Value::Null } else { json!(id) },
            "model_id": model_id.get_untracked().trim(),
            "files": files,
            "args": args,
            "modes": modes,
            "edit": edit.get_untracked(),
            "enabled": enabled.get_untracked(),
            "idle_seconds": idle_v,
            "image": image.get_untracked().trim(),
            "extra_run_args": if era_empty { Value::Null } else { json!(era) },
            "warm_start": warm_start.get_untracked(),
            "hold_fallback_mode": hf_mode,
            "hold_fallback": hf_alias,
            "capabilities_override": capabilities_override.get_untracked().trim(),
            "clear": clear.join(","),
        });
        saving.set(true);
        let saved_id = model_id.get_untracked().trim().to_string();
        spawn_local(async move {
            let res = crate::api::post::<Value, _>("/api/op/image_model_set", &body).await;
            saving.set(false);
            match res {
                Ok(v) => {
                    // Saved: the modal stays open on the command line, and
                    // closing it now is not a discard (review par:PAR-4).
                    if let Some(m) = modal {
                        m.saved();
                    }
                    saved_once.set(true);
                    save_warnings.set(
                        v.get("warnings")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|w| w.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    );
                    save_command_line.set(
                        v.get("command_line")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    );
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("saved")
                        .to_string());
                    // The row exists now (or was renamed), so the read-back
                    // below has something to read — the modal stays open on
                    // purpose, because the command line it just rendered is
                    // the thing worth looking at.
                    saved_model_id.set(saved_id);
                    detail_reload.update(|n| *n += 1);
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
        {move || {
            let warnings = save_warnings.get();
            (!warnings.is_empty())
                .then(|| {
                    view! {
                        <div class="notice warn editor-top">
                            <b>"Saved with warnings"</b>
                            {warnings
                                .iter()
                                .map(|w| view! { <span class="detail">{w.clone()}</span> })
                                .collect_view()}
                        </div>
                    }
                })
        }}
        <div class="card-flow editor-flow">
        <section class="card edit-section">
            <h3>"Model"</h3>
            <div class="field">
                <label>"Model id"</label>
                <input class="input mono"
                    prop:value=move || model_id.get()
                    on:input=move |ev| model_id.set(event_target_value(&ev))/>
            </div>
            <div class="row" style="margin-top:8px">
                <label class="row dim" style="gap:5px">
                    <input
                        type="checkbox"
                        prop:checked=move || img_gen.get()
                        on:change=move |ev| img_gen.set(event_target_checked(&ev))
                    />
                    "img_gen"
                </label>
                <label class="row dim" style="gap:5px">
                    <input
                        type="checkbox"
                        prop:checked=move || vid_gen.get()
                        on:change=move |ev| vid_gen.set(event_target_checked(&ev))
                    />
                    "vid_gen"
                </label>
                <label class="row dim" style="gap:5px">
                    <input
                        type="checkbox"
                        prop:checked=move || edit.get()
                        on:change=move |ev| edit.set(event_target_checked(&ev))
                    />
                    "serves /v1/images/edits (takes reference images)"
                </label>
            </div>
            <p class="dim mini-note">
                "Only tick edits for a pipeline that really takes reference images: measured "
                "(design §12.8), one that does not does not refuse the request — sd-server "
                "segfaults and the container exits."
            </p>
            {move || {
                let endpoints = detail
                    .get()
                    .flatten()
                    .map(|d| d.endpoints)
                    .unwrap_or_default();
                (!endpoints.is_empty())
                    .then(|| {
                        view! {
                            <div class="dim mini-note mono-sm">
                                {format!("serving {}", endpoints.join(", "))}
                            </div>
                        }
                    })
            }}
        </section>

        <section class="card edit-section">
            <h3>"Pipeline files"</h3>
            <div class="row">
                <label class="row dim" style="gap:5px; white-space:nowrap">
                    <input
                        type="radio"
                        name="image-entry"
                        prop:checked=move || entry_key.get() == "diffusion_model"
                        on:change=move |_| entry_key.set("diffusion_model".into())
                    />
                    "component pipeline (--diffusion-model)"
                </label>
                <label class="row dim" style="gap:5px; white-space:nowrap">
                    <input
                        type="radio"
                        name="image-entry"
                        prop:checked=move || entry_key.get() == "model"
                        on:change=move |_| entry_key.set("model".into())
                    />
                    "all-in-one checkpoint (--model)"
                </label>
            </div>
            <ImageFileField
                label="Diffusion model"
                value=diffusion_path
                role="diffusion"
                present=detail
                key=Signal::derive(|| "diffusion_model".to_string())
                disabled=Signal::derive(move || entry_key.get() != "diffusion_model")
            />
            <ImageFileField
                label="Checkpoint"
                value=checkpoint_path
                role="checkpoint"
                present=detail
                key=Signal::derive(|| "model".to_string())
                disabled=Signal::derive(move || entry_key.get() != "model")
            />
            <p class="dim mini-note">
                "Exactly one of the two loads the pipeline; everything below hangs off it. "
                "Paths are relative to the image models dir (Settings → Runtimes → Image)."
            </p>
            {IMAGE_FILE_ROLES
                .iter()
                .enumerate()
                .map(|(i, (key, label, role))| {
                    let sig = role_sigs.with_value(|rs| rs[i].1);
                    let key: &'static str = key;
                    view! {
                        <Show when=move || shown_roles.with(|s| s.contains(&key))>
                            <ImageFileField
                                label=*label
                                value=sig
                                role=*role
                                present=detail
                                key=Signal::derive(move || key.to_string())
                                disabled=Signal::derive(|| false)
                                on_remove=Callback::new(move |()| {
                                    sig.set(String::new());
                                    shown_roles.update(|s| s.retain(|k| *k != key));
                                })
                            />
                        </Show>
                    }
                })
                .collect_view()}
            {move || {
                other_files
                    .get()
                    .into_iter()
                    .enumerate()
                    .map(|(i, (k, v))| {
                        view! {
                            <div class="field">
                                <label>"Other role" <span class="field-unit">"its --help key, underscores or hyphens"</span></label>
                                <div class="row" style="flex-wrap:nowrap">
                                    <input
                                        class="input mono w-sm"
                                        placeholder="flag key"
                                        prop:value=k
                                        on:input=move |ev| {
                                            let val = event_target_value(&ev);
                                            other_files.update(|rows| rows[i].0 = val);
                                        }
                                    />
                                    <input
                                        class="input mono"
                                        style="flex:1"
                                        placeholder="path under the image models dir"
                                        prop:value=v
                                        on:input=move |ev| {
                                            let val = event_target_value(&ev);
                                            other_files.update(|rows| rows[i].1 = val);
                                        }
                                    />
                                    <button
                                        class="btn ghost"
                                        title="Remove this role"
                                        on:click=move |_| {
                                            other_files.update(|rows| { rows.remove(i); });
                                            touch_editor();
                                        }
                                    >
                                        "✕"
                                    </button>
                                </div>
                            </div>
                        }
                    })
                    .collect_view()
            }}
            <div class="add-role">
                <RowMenu items=add_role_items label=add_role_label title="Name another file this pipeline loads"/>
            </div>
        </section>

        <section class="card edit-section">
            <h3>"Generation defaults"</h3>
            <div class="spec-grid">
                {IMAGE_ARGS
                    .iter()
                    .filter(|a| !a.switch)
                    .enumerate()
                    .map(|(i, a)| {
                        let sig = arg_sigs.with_value(|s| s[i].1);
                        let type_opts = Signal::derive(|| {
                            std::iter::once((String::new(), "sd-server default".to_string()))
                                .chain(
                                    IMAGE_TYPES.iter().map(|t| (t.to_string(), t.to_string())),
                                )
                                .collect::<Vec<_>>()
                        });
                        view! {
                            <div class="field">
                                <label title=a.hint>
                                    {a.label}
                                    {(!a.hint.is_empty())
                                        .then(|| {
                                            view! {
                                                <span class="dim">
                                                    {format!("  ·  {}", a.hint)}
                                                </span>
                                            }
                                        })}
                                </label>
                                {if a.key == "type" {
                                    view! { <Select value=sig options=type_opts/> }.into_any()
                                } else {
                                    view! {
                                        <input
                                            class="input mono"
                                            prop:value=move || sig.get()
                                            on:input=move |ev| sig.set(event_target_value(&ev))
                                        />
                                    }
                                        .into_any()
                                }}
                            </div>
                        }
                    })
                    .collect_view()}
            </div>
            {IMAGE_ARGS
                .iter()
                .filter(|a| a.switch)
                .enumerate()
                .map(|(i, a)| {
                    let sig = switch_sigs.with_value(|s| s[i].1);
                    view! {
                        <label class="row dim" style="gap:5px; margin-top:8px">
                            <input
                                type="checkbox"
                                prop:checked=move || sig.get()
                                on:change=move |ev| sig.set(event_target_checked(&ev))
                            />
                            {a.label}
                            {(!a.hint.is_empty())
                                .then(|| view! { <span class="dim">{format!(" · {}", a.hint)}</span> })}
                        </label>
                    }
                })
                .collect_view()}
            <p class="dim mini-note">
                "Offloading measured on the spike pipeline (design §12.4): idle 1.0 GiB / peak "
                "7.7 GiB at 1024² instead of 7.1 / 13.7, and +0.4 s per image."
            </p>
            <div class="field" style="margin-top:8px">
                <label>"Raw args (JSON object — merged last, a key here wins)"</label>
                <textarea class="input mono ta" prop:value=move || raw_args.get()
                    on:input=move |ev| raw_args.set(event_target_value(&ev))></textarea>
            </div>
        </section>

        <section class="card edit-section">
            <h3>"Container"</h3>
            <div class="row">
                <label class="row dim" style="gap:5px">
                    <input
                        type="checkbox"
                        prop:checked=move || enabled.get()
                        on:change=move |ev| enabled.set(event_target_checked(&ev))
                    />
                    "enabled"
                </label>
                <label class="row dim" style="gap:5px">
                    <input
                        type="checkbox"
                        prop:checked=move || warm_start.get()
                        on:change=move |ev| warm_start.set(event_target_checked(&ev))
                    />
                    "start at boot (warm start)"
                </label>
            </div>
            <div class="field" style="margin-top:8px">
                <label>"Idle seconds before sleep (0 = never)"</label>
                <input class="input mono" prop:value=move || idle.get()
                    on:input=move |ev| idle.set(event_target_value(&ev))/>
            </div>
            <div class="field" style="margin-top:8px">
                <label>"Hold fallback"</label>
                <HoldFallback
                    mode=hold_mode
                    alias=hold_alias
                    inherit_label="Inherit · none"
                    tasks=&["image_generation", "image_edit"]
                />
            </div>
            <p class="dim mini-note">
                "Where this model's requests go while a GPU hold (Settings → GPU → Hold) is active. "
                "Image models never inherit the global fallback, only their own row does, so "
                "\"Inherit\" here means the same as \"None\": refuse with a 503."
            </p>
            <div class="field" style="margin-top:8px">
                <label>"Image override"</label>
                <ImagePicker
                    value=image
                    class=ImageClass::Image
                    clearable=true
                    placeholder=Signal::derive(move || {
                        class_settings
                            .get()
                            .and_then(|r| r.ok())
                            .map(|s| s.image.image)
                            .unwrap_or_default()
                    })
                />
            </div>
            <div class="field" style="margin-top:8px">
                <label>"Extra podman run args override (one per line)"</label>
                <textarea
                    class="input mono ta"
                    placeholder=move || {
                        class_settings
                            .get()
                            .and_then(|r| r.ok())
                            .map(|s| s.image.extra_run_args.join("\n"))
                            .unwrap_or_default()
                    }
                    prop:value=move || extra_run_args.get()
                    on:input=move |ev| extra_run_args.set(event_target_value(&ev))
                ></textarea>
            </div>
            <p class="dim mini-note">
                "Blank image/args inherit the image class settings (Settings → Runtimes → Image)." " " {run_args_note::OVERRIDE_REPLACES}
            </p>
            <ContainerStatusRow class="image" model_id=saved_model_id/>
        </section>

        <section class="card edit-section">
            <h3>"Owner overrides"</h3>
            <div class="field">
                <label>"Capability override (JSON)"</label>
                <textarea class="input mono ta" prop:value=move || capabilities_override.get()
                    on:input=move |ev| capabilities_override.set(event_target_value(&ev))></textarea>
            </div>
            <p class="dim mini-note">
                "Merged over what lmgw derives for /v1/models — optional keys capabilities "
                "(deep-merged), max_output_tokens, notes (appended). Blank clears."
            </p>
        </section>

        {move || {
            let line = match save_command_line.get() {
                l if !l.is_empty() => l,
                _ => detail.get().flatten().map(|d| d.command_line).unwrap_or_default(),
            };
            (!line.is_empty())
                .then(|| {
                    view! {
                        <section class="card edit-section">
                            <h3>"Rendered command line"</h3>
                            <pre
                                class="preset"
                                style="white-space:pre-wrap; word-break:break-all"
                            >
                                {line}
                            </pre>
                            <div class="dim mini-note">
                                "Exactly what this model's own container is started with; the "
                                "published port is allocated per start."
                            </div>
                        </section>
                    }
                })
        }}
        </div>

        <ModalFooter>
            // "Cancel" like every other editor's, until a save leaves the
            // modal open on the command line it rendered: then it is "done
            // here". Either way unsaved input is asked about, as ✕ does.
            <button
                class="btn ghost"
                on:click=move |_| match modal {
                    Some(m) => m.request_close(),
                    None => open.set(false),
                }
            >
                {move || if saved_once.get() { "Close" } else { "Cancel" }}
            </button>
            <button class="btn primary" disabled=move || saving.get() on:click=save>
                {move || if saving.get() { "Saving…" } else { "Save model" }}
            </button>
        </ModalFooter>
    }
}

/// One `files` role: a path input, a browse modal over the image models dir,
/// and the server's verdict on whether the named path is actually there.
///
/// The browse list is the whole image dir (`/api/gguf-files?target=image`
/// lists every kind sd-server loads, not just GGUF) with this role's guessed
/// kind sorted to the top — never filtered to it, because `role_guess` is a
/// filename heuristic and a pipeline is allowed to disagree with it.
#[component]
fn ImageFileField(
    label: &'static str,
    value: RwSignal<String>,
    role: &'static str,
    present: LocalResource<Option<ImageModelDetail>>,
    /// The `files` key this field writes, for the presence lookup. A signal
    /// because the entry-point field's key follows the checkpoint/component
    /// radio.
    #[prop(into)]
    key: Signal<String>,
    #[prop(into)] disabled: Signal<bool>,
    /// A ✕ that takes the role off the form (clearing its path).
    #[prop(optional)]
    on_remove: Option<Callback<()>>,
) -> impl IntoView {
    // The editor this field sits in (not the browser below, which is a modal
    // of its own): a pick or a ✕ writes the path without an input event, and
    // has to tell it so, or Esc would drop the pick unasked.
    let editor = crate::widgets::use_modal();
    let touch = move || {
        if let Some(m) = editor {
            m.touch();
        }
    };
    let open = RwSignal::new(false);
    let files = LocalResource::new(move || {
        let want = open.get();
        async move {
            if !want {
                return None;
            }
            crate::api::get::<GgufFiles>("/api/gguf-files?target=image")
                .await
                .ok()
        }
    });
    // `false` only when the server looked and did not find it; an absent key
    // (never saved, or cleared) says nothing and shows nothing.
    let missing = move || {
        let k = key.get();
        present
            .get()
            .flatten()
            .and_then(|d| d.files_present.get(&k).copied())
            == Some(false)
    };
    view! {
        <div class="field" style="margin-top:8px">
            <label>
                {label}
                {move || {
                    missing()
                        .then(|| {
                            view! {
                                <span class="problem" style="display:inline; margin-left:8px">
                                    "not on disk"
                                </span>
                            }
                        })
                }}
            </label>
            <div class="row" style="flex-wrap:nowrap">
                <input
                    class="input mono"
                    style="flex:1"
                    disabled=move || disabled.get()
                    prop:value=move || value.get()
                    on:input=move |ev| value.set(event_target_value(&ev))
                />
                <button
                    class="btn"
                    disabled=move || disabled.get()
                    on:click=move |_| open.set(true)
                >
                    "Browse"
                </button>
                {on_remove
                    .map(|cb| {
                        view! {
                            <button
                                class="btn ghost"
                                title="Take this role off the pipeline"
                                on:click=move |_| {
                                    cb.run(());
                                    touch();
                                }
                            >
                                "✕"
                            </button>
                        }
                    })}
            </div>
            <Modal open=open title="Files in the image models dir" fill=true>
                {move || match files.get().flatten() {
                    None => view! { <div class="dim">"Scanning…"</div> }.into_any(),
                    Some(gf) => {
                        let mut fs = gf.files;
                        fs.sort_by(|a, b| {
                            (a.role_guess != role)
                                .cmp(&(b.role_guess != role))
                                .then(a.path.cmp(&b.path))
                        });
                        view! {
                            <div class="wiz-files">
                                <For each=move || fs.clone() key=|f| f.path.clone() let:f>
                                    <button
                                        class="wiz-file"
                                        on:click={
                                            let path = f.path.clone();
                                            move |_| {
                                                value.set(path.clone());
                                                touch();
                                                open.set(false);
                                            }
                                        }
                                    >
                                        <span class="mono-sm">{f.path.clone()}</span>
                                        <span class="type-badge">{f.role_guess.clone()}</span>
                                        {(!f.used_by.is_empty())
                                            .then(|| {
                                                view! {
                                                    <span class="dim">
                                                        {format!("used by {}", f.used_by.join(", "))}
                                                    </span>
                                                }
                                            })}
                                        <span class="spacer" style="flex:1"></span>
                                        <span class="dim mono-sm">{f.size.clone()}</span>
                                    </button>
                                </For>
                            </div>
                        }
                            .into_any()
                    }
                }}
            </Modal>
        </div>
    }
}
