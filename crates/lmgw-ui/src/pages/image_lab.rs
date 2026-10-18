//! Image lab — the Audio lab's sibling for stable-diffusion.cpp
//! (image-generation design §8): pick a model that draws, fill the form its
//! pipeline understands, fire the request, look at the picture.
//!
//! The two panels post to `/image-lab/api/*`, which are thin in-process
//! wrappers over the *real* `/v1/images/generations` and `/v1/images/edits`
//! handlers — so what this page exercises is the gateway passthrough, guards,
//! GPU hold, admission, error normalization and the `class = image` log row
//! included. Upstream failures are therefore rendered from the envelope's own
//! `message` and `code` rather than guessed at.
//!
//! The request document is built by [`lmgw_api_types::image_lab::ImageGenForm`]
//! — the same function the server dispatches with, so the "Request" panel shows
//! what will actually be sent and not a second opinion about it.
//!
//! Every generation and edit is kept on the server (labs-store design §5):
//! the page keeps the form as a group first (`/image-lab/api/groups`,
//! `/groups/edit`), puts it in front of the history in flight, then runs it
//! (`/groups/{id}/run`) and puts the answered group in its place. A Stop
//! reads the group again until it is no longer in flight, so a picture the
//! route answered meanwhile still lands. The results column is the lab's
//! kept history, read through `pages/lab_history` and drawn by [`card`],
//! each picture a file the lab store serves. A group goes back into the form ("Open in form"), a kept
//! picture into the edit panel without an upload, and the form's prompt and
//! settings can be saved as named presets.
//!
//! Two things the OpenAI image routes cannot say, and how they are said here:
//! everything beyond `prompt`/`n`/`size`/`output_format`/`output_compression`
//! (§2.3) rides in sd.cpp's own `<sd_cpp_extra_args>` prompt block, and a LoRA
//! is the structured `lora` field inside it — `<lora:…>` prompt tags are
//! refused by every family. The sampler and scheduler lists come from the
//! container's own `capabilities` probe; before it has ever run there is no
//! list, and the page says so instead of inventing one.

use gloo_net::http::Request;
use leptos::prelude::*;
use leptos_router::components::A;
use lmgw_api_types::image_lab::{ImageGenForm, ImageLoraRow, EDITS_ENDPOINT, GENERATIONS_ENDPOINT};
use lmgw_api_types::image_lab::{ImageLabModel, ImageLabModels};
use lmgw_api_types::lab_store::{LabFileRef, LabGroupView, LabKind, LabPresetBody, LabPresetKind};
use lmgw_api_types::ImageCapabilities;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::lab_frame::{Check, Fname, LabFrame, LabView, TaskGroup, TaskMenu, TaskOpt};
use super::lab_history::{ExportMenu, HistoryList, LabHistory};
use super::lab_presets::PresetsCard;
use crate::catalog::CatalogEntry;
use crate::scope::Scope;
use crate::widgets::model_picker::ListStatus;
use crate::widgets::{Explain, ModelPicker, Select};

/// A kept generation's or edit's card.
mod card;
/// The form and the kept groups: Open in form, presets, Send to edit, the
/// edit's upload.
mod kept;

use card::{ImageCard, StrayCard};

// ---------------------------------------------------------------------------
// Server shapes
// ---------------------------------------------------------------------------

// `GET /image-lab/api/models` is `lmgw_api_types::image_lab`'s answer — the
// type the gateway builds — so a renamed field breaks this build, not the page.
type LabModel = ImageLabModel;
type ModelsResp = ImageLabModels;

// ---------------------------------------------------------------------------
// Fetch helpers
//
// The `/image-lab/api/*` plane answers failures with the *gateway's* envelope
// (`{"error": {"message", "type", "code"}}`), not the admin plane's
// `{code, message}` — so `crate::api` would swallow the message.
// ---------------------------------------------------------------------------

/// The message and code out of an error body, falling back to the raw text and
/// finally the status line.
fn error_parts(text: &str, status: u16) -> (String, String) {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        let code = v["error"]["code"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_default();
        if let Some(m) = v["error"]["message"].as_str().filter(|s| !s.is_empty()) {
            return (m.to_string(), code);
        }
        if let Some(m) = v["error"].as_str().filter(|s| !s.is_empty()) {
            return (m.to_string(), code);
        }
    }
    if text.trim().is_empty() {
        (format!("HTTP {status}"), String::new())
    } else {
        (text.to_string(), String::new())
    }
}

async fn lab_get<T: DeserializeOwned>(url: &str) -> Result<T, String> {
    let resp = Request::get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if (200..300).contains(&status) {
        serde_json::from_str(&text).map_err(|e| format!("bad response body: {e}"))
    } else {
        Err(error_parts(&text, status).0)
    }
}

fn now_hms() -> String {
    let d = js_sys::Date::new_0();
    format!(
        "{:02}:{:02}:{:02}",
        d.get_hours(),
        d.get_minutes(),
        d.get_seconds()
    )
}

// ---------------------------------------------------------------------------
// Page state (Copy bundles of signals, so handlers and child components can
// capture them freely)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Form {
    prompt: RwSignal<String>,
    negative_prompt: RwSignal<String>,
    width: RwSignal<String>,
    height: RwSignal<String>,
    n: RwSignal<String>,
    steps: RwSignal<String>,
    cfg_scale: RwSignal<String>,
    seed: RwSignal<String>,
    sampler: RwSignal<String>,
    scheduler: RwSignal<String>,
    output_format: RwSignal<String>,
    output_compression: RwSignal<String>,
    /// `(id, path, multiplier)` — a plain tuple rather than nested signals, so
    /// a keyed `<For>` leaves a row's inputs alone while it is being typed in.
    loras: RwSignal<Vec<(u32, String, String)>>,
    next_lora: RwSignal<u32>,
    // Edit panel
    image_file: StoredValue<Option<web_sys::File>>,
    image_name: RwSignal<String>,
    mask_file: StoredValue<Option<web_sys::File>>,
    mask_name: RwSignal<String>,
    /// A kept picture in the image slot (an opened edit's, a result sent to
    /// edit): named by its artifact, sent without an upload.
    image_ref: RwSignal<Option<LabFileRef>>,
    mask_ref: RwSignal<Option<LabFileRef>>,
}

impl Form {
    fn new() -> Self {
        Self {
            prompt: RwSignal::new(String::new()),
            negative_prompt: RwSignal::new(String::new()),
            width: RwSignal::new(String::new()),
            height: RwSignal::new(String::new()),
            n: RwSignal::new(String::new()),
            steps: RwSignal::new(String::new()),
            cfg_scale: RwSignal::new(String::new()),
            seed: RwSignal::new(String::new()),
            sampler: RwSignal::new(String::new()),
            scheduler: RwSignal::new(String::new()),
            output_format: RwSignal::new(String::new()),
            output_compression: RwSignal::new(String::new()),
            loras: RwSignal::new(Vec::new()),
            next_lora: RwSignal::new(1),
            image_file: StoredValue::new(None),
            image_name: RwSignal::new(String::new()),
            mask_file: StoredValue::new(None),
            mask_name: RwSignal::new(String::new()),
            image_ref: RwSignal::new(None),
            mask_ref: RwSignal::new(None),
        }
    }
}

#[derive(Clone, Copy)]
struct Lab {
    models: RwSignal<Vec<LabModel>>,
    models_dir: RwSignal<String>,
    hold: RwSignal<bool>,
    alias: RwSignal<String>,
    /// `false` = the generation panel, `true` = the edit panel. Only reachable
    /// for a model that advertises `/v1/images/edits`.
    editing: RwSignal<bool>,
    form: Form,
    /// Where the model list stands (the picker says so while it loads).
    list: RwSignal<ListStatus>,
    running: RwSignal<bool>,
    /// The request being kept as a group: model, route and start time, for
    /// the card it has until the group is made.
    pending: RwSignal<Option<(String, &'static str, String)>>,
    /// The group whose request this page has out: its card is the live one.
    live: RwSignal<Option<i64>>,
    /// An answer whose group is no longer in the history (a Clear while it
    /// ran), with why it was not kept: its card stays above the history
    /// until the next run.
    stray: RwSignal<Option<(LabGroupView, String)>>,
    /// The kept history (`/labs/api/image/*`).
    history: LabHistory,
    /// A model "Open in form" or a preset just picked: its row's own
    /// defaults are not put over the fields loaded with it.
    loaded: StoredValue<Option<String>>,
    view: RwSignal<LabView>,
    aborter: StoredValue<Option<web_sys::AbortController>>,
    /// The page's lifetime: a read or a run that answers after the lab was
    /// left ends there (review code:C4).
    scope: Scope,
    toasts: crate::widgets::Toasts,
}

impl Lab {
    /// Stop the request out: its fetch is aborted, and its group read again
    /// until it is no longer in flight.
    fn stop(self) {
        if let Some(a) = self.aborter.get_value() {
            a.abort();
        }
    }

    fn model(&self) -> Option<LabModel> {
        let alias = self.alias.get();
        self.models.get().into_iter().find(|m| m.name == alias)
    }

    /// What the chosen model's container reports about the pipeline it loaded.
    ///
    /// The live runtime frame wins over the list's snapshot: a container that
    /// came up while this page was open has already broadcast its capabilities,
    /// and the sampler select should fill itself without a reload.
    fn caps(&self) -> Option<ImageCapabilities> {
        let m = self.model()?;
        if let Some(model_id) = m.model_id.clone() {
            let live = crate::live::use_live();
            if let Some(rows) = live.runtime.get() {
                if let Some(r) = rows
                    .into_iter()
                    .find(|r| r.class == "image" && r.model_id == model_id)
                {
                    return r.image_capabilities;
                }
            }
        }
        m.image_capabilities
    }

    /// Container state for the chosen model, live frame first. `None` for a
    /// cloud alias — there is nothing of ours to be up.
    fn state(&self) -> Option<String> {
        let m = self.model()?;
        let model_id = m.model_id.clone()?;
        let live = crate::live::use_live();
        if let Some(rows) = live.runtime.get() {
            return rows
                .into_iter()
                .find(|r| r.class == "image" && r.model_id == model_id)
                .map(|r| r.state);
        }
        m.state
    }

    fn warnings(&self) -> Vec<String> {
        let Some(m) = self.model() else {
            return Vec::new();
        };
        if let (Some(model_id), Some(rows)) =
            (m.model_id.clone(), crate::live::use_live().runtime.get())
        {
            if let Some(r) = rows
                .into_iter()
                .find(|r| r.class == "image" && r.model_id == model_id)
            {
                return r.warnings;
            }
        }
        m.warnings
    }

    fn can_edit(&self) -> bool {
        self.model().map(|m| m.edit).unwrap_or(false)
    }

    fn endpoint(&self) -> &'static str {
        if self.editing.get() {
            EDITS_ENDPOINT
        } else {
            GENERATIONS_ENDPOINT
        }
    }

    /// The form as the server will read it.
    fn spec(&self) -> ImageGenForm {
        let f = self.form;
        ImageGenForm {
            model: self.alias.get(),
            prompt: f.prompt.get(),
            negative_prompt: f.negative_prompt.get(),
            width: f.width.get(),
            height: f.height.get(),
            n: f.n.get(),
            steps: f.steps.get(),
            cfg_scale: f.cfg_scale.get(),
            seed: f.seed.get(),
            sampler: f.sampler.get(),
            scheduler: f.scheduler.get(),
            output_format: f.output_format.get(),
            output_compression: f.output_compression.get(),
            loras: f
                .loras
                .get()
                .into_iter()
                .map(|(_, path, multiplier)| ImageLoraRow { path, multiplier })
                .collect(),
        }
    }

    /// The action bar's line: what a run would call, or what is missing —
    /// the refusals the run itself makes, read live.
    fn check(&self) -> Check {
        if self.models.with(Vec::is_empty) {
            return Check::Needs(match self.list.get() {
                ListStatus::Loading => "loading the models that draw…".into(),
                _ => "no model here can draw".into(),
            });
        }
        let spec = self.spec();
        if spec.model.trim().is_empty() {
            return Check::Needs("pick a model".into());
        }
        if spec.prompt.trim().is_empty() {
            return Check::Needs("a prompt is required".into());
        }
        if let Err(e) = spec.generation_body() {
            return Check::Invalid(e);
        }
        if self.editing.get() && self.form.image_name.with(String::is_empty) {
            return Check::Needs("pick an image to edit".into());
        }
        let local = self.model().is_some_and(|m| m.local);
        let mut line = self.endpoint().to_string();
        if local && self.hold.get() {
            line.push_str(" · GPU held: the fallback answers or it refuses");
        } else if local && self.state().as_deref() != Some("ready") {
            line.push_str(" · the first run starts the pipeline");
        }
        Check::Ready(line)
    }

    /// The exact outgoing document — built by the function the server
    /// dispatches with, so the panel can never drift from the wire.
    fn preview(&self) -> String {
        let spec = self.spec();
        if self.editing.get() {
            let fields = match spec.edit_fields() {
                Ok(f) => f,
                Err(e) => return format!("⚠ {e}"),
            };
            let f = self.form;
            let mut out: Vec<String> = fields
                .iter()
                .map(|(k, v)| format!("{k} = {v}"))
                .collect::<Vec<_>>();
            let kept = |r: &Option<LabFileRef>| {
                r.as_ref()
                    .map(|r| format!(" (kept, lab-artifact:{})", r.id))
                    .unwrap_or_default()
            };
            out.push(format!(
                "image = {}{}",
                some_or(&f.image_name.get(), "⚠ no file picked"),
                kept(&f.image_ref.get())
            ));
            if !f.mask_name.get().is_empty() {
                out.push(format!(
                    "mask = {}{}",
                    f.mask_name.get(),
                    kept(&f.mask_ref.get())
                ));
            }
            return out.join("\n");
        }
        match spec.generation_body() {
            Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
            Err(e) => format!("⚠ {e}"),
        }
    }
}

fn some_or(s: &str, fallback: &str) -> String {
    if s.trim().is_empty() {
        fallback.to_string()
    } else {
        s.to_string()
    }
}

/// A row's `args` value as text: `--steps 8` and `--steps "8"` are the same
/// number here, and a switch has no value to prefill with.
fn arg_text(args: &Option<serde_json::Map<String, Value>>, key: &str) -> String {
    match args.as_ref().and_then(|a| a.get(key)) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

/// A concrete, reusable seed for the **Random** button.
///
/// The bound is sd-server's own: its `seed` is a C `int`, so `i32::MAX` is the
/// largest value that survives the round trip — the number is the type's, not
/// an eyeballed constant that happens to look large enough. The capabilities
/// document publishes no seed range today; the day a build reports one, it is
/// what belongs here.
fn random_seed() -> i64 {
    (js_sys::Math::random() * f64::from(i32::MAX)) as i64
}

#[component]
pub fn ImageLab() -> impl IntoView {
    let scope = Scope::new();
    let lab = Lab {
        models: RwSignal::new(Vec::new()),
        models_dir: RwSignal::new(String::new()),
        hold: RwSignal::new(false),
        alias: RwSignal::new(String::new()),
        editing: RwSignal::new(false),
        form: Form::new(),
        list: RwSignal::new(ListStatus::Loading),
        running: RwSignal::new(false),
        pending: RwSignal::new(None),
        live: RwSignal::new(None),
        stray: RwSignal::new(None),
        history: LabHistory::new(LabKind::Image, scope),
        loaded: StoredValue::new(None),
        view: RwSignal::new(LabView::Form),
        aborter: StoredValue::new(None),
        toasts: crate::widgets::use_toasts(),
        scope,
    };

    let load_models = move || {
        lab.scope.spawn(async move {
            match lab_get::<ModelsResp>("/image-lab/api/models").await {
                Ok(m) => {
                    lab.models_dir.set(m.models_dir);
                    lab.hold.set(m.hold);
                    let first = m.models.first().map(|f| f.name.clone());
                    lab.models.set(m.models);
                    lab.list.set(ListStatus::Ready);
                    if lab.alias.get_untracked().is_empty() {
                        if let Some(alias) = first {
                            lab.alias.set(alias);
                        }
                    }
                }
                Err(e) => {
                    lab.models.set(Vec::new());
                    lab.list.set(ListStatus::Failed(e));
                }
            }
        });
    };
    load_models();

    // Model change → adopt the defaults its row already carries. One-way (the
    // picker never writes the alias back), so no notify-on-equal ping-pong.
    //
    // The size, step and CFG defaults are the flags the container is *started*
    // with — the row's own argv — and nothing else: a model whose row says
    // nothing leaves the fields empty, where "empty" means "let the server use
    // its own default" rather than a number this page made up.
    Effect::new(move |prev: Option<String>| {
        let alias = lab.alias.get();
        if prev.as_deref() == Some(alias.as_str()) {
            return alias;
        }
        let Some(m) = lab
            .models
            .get_untracked()
            .into_iter()
            .find(|m| m.name == alias)
        else {
            return alias;
        };
        if !m.edit {
            lab.editing.set(false);
        }
        // Loaded with its fields: the row's defaults stay out of them.
        if lab
            .loaded
            .try_update_value(Option::take)
            .flatten()
            .as_deref()
            == Some(alias.as_str())
        {
            return alias;
        }
        let f = lab.form;
        f.width.set(arg_text(&m.args, "width"));
        f.height.set(arg_text(&m.args, "height"));
        f.steps.set(arg_text(&m.args, "steps"));
        f.cfg_scale.set(arg_text(&m.args, "cfg_scale"));
        f.sampler.set(arg_text(&m.args, "sampling_method"));
        f.scheduler.set(arg_text(&m.args, "scheduler"));
        alias
    });

    let stop = move || lab.stop();

    let run = move || {
        if lab.running.get_untracked() {
            return;
        }
        let spec = untrack(|| lab.spec());
        let editing = lab.editing.get_untracked();
        let endpoint = untrack(|| lab.endpoint());
        let f = lab.form;
        let image = (f.image_file.get_value(), f.image_ref.get_untracked());
        let mask = (f.mask_file.get_value(), f.mask_ref.get_untracked());
        // The same refusals the action bar shows, before a container is
        // started for a request that cannot be made.
        let refusal = if spec.model.trim().is_empty() {
            Some("pick a model first".to_string())
        } else if let Err(e) = spec.generation_body() {
            Some(e)
        } else if editing && image.0.is_none() && image.1.is_none() {
            Some("pick an image to edit".to_string())
        } else {
            None
        };
        if let Some(e) = refusal {
            lab.toasts.err(e);
            return;
        }
        lab.view.set(LabView::Results);
        lab.stray.set(None);

        lab.running.set(true);
        lab.pending
            .set(Some((spec.model.trim().to_string(), endpoint, now_hms())));
        let ctrl = web_sys::AbortController::new().ok();
        let signal = ctrl.as_ref().map(|c| c.signal());
        lab.aborter.set_value(ctrl);
        // A run the lab was left during ends with it; the server keeps it.
        lab.scope.spawn(async move {
            match kept::make_group(&spec, editing, image, mask, signal.as_ref()).await {
                Ok(g) => {
                    let id = g.id;
                    lab.live.set(Some(id));
                    lab.history.prepend(g);
                    lab.pending.set(None);
                    kept::run_group(lab, id, signal.as_ref()).await;
                    // A first request starts the container; its capability
                    // probe is what fills the sampler lists.
                    load_models();
                }
                Err(e) if kept::is_abort(&e) => {}
                Err(e) => lab.toasts.err(e),
            }
            lab.history.refresh_usage();
            lab.live.set(None);
            lab.pending.set(None);
            lab.running.set(false);
            lab.aborter.set_value(None);
        });
    };

    on_cleanup(move || {
        lab.aborter
            .try_with_value(|a| a.as_ref().map(web_sys::AbortController::abort));
    });

    let entries = Signal::derive(move || {
        lab.models
            .get()
            .iter()
            .map(|m| CatalogEntry {
                id: m.name.clone(),
                source: if m.local {
                    "sdcpp".into()
                } else {
                    m.owner.clone()
                },
                group_label: if m.local {
                    "sd.cpp".into()
                } else {
                    m.owner.clone()
                },
                vendor: None,
                task: Some(m.task.clone()),
                ctx: None,
                price_in: None,
                price_out: None,
                price_varies: false,
                vision: Some(false),
                tools: false,
                reasoning: false,
                reasoning_facts: None,
                local: m.local,
                // Listed because it serves /v1/images/generations.
                routable: true,
            })
            .collect::<Vec<_>>()
    });
    let routes = Signal::derive(move || {
        let edit_off = (!lab.can_edit()).then(|| {
            if lab.model().is_some_and(|m| m.local) {
                "this row has edit = false: its pipeline takes no reference image".to_string()
            } else {
                "its catalog lists no image input, so edits are not offered".to_string()
            }
        });
        vec![TaskGroup {
            label: "",
            opts: vec![
                TaskOpt {
                    id: "generate".into(),
                    label: "Generate".into(),
                    tag: GENERATIONS_ENDPOINT.into(),
                    native: false,
                    off: None,
                },
                TaskOpt {
                    id: "edit".into(),
                    label: "Edit".into(),
                    tag: EDITS_ENDPOINT.into(),
                    native: false,
                    off: edit_off,
                },
            ],
        }]
    });
    let preview = Memo::new(move |_| lab.preview());
    let check = Memo::new(move |_| lab.check());
    let filter_models = Signal::derive(move || {
        lab.models
            .with(|v| v.iter().map(|m| m.name.clone()).collect::<Vec<_>>())
    });
    let card = Callback::new(move |id: i64| view! { <ImageCard lab=lab id=id/> }.into_any());
    let preset_now =
        Callback::new(move |()| LabPresetBody::ImagePrompt(Box::new(untrack(|| lab.spec()))));
    let preset_load = Callback::new(move |b: LabPresetBody| {
        if let LabPresetBody::ImagePrompt(f) = b {
            lab.load_form(&f);
        }
    });

    view! {
        <LabFrame
            persist="image-lab.pipeline"
            side_label="Pipeline"
            side_badge=Signal::derive(String::new)
            side=move || view! { <SidePanel lab=lab/> }
            head=move || {
                view! {
                    <span class="lab-label">"Model"</span>
                    <ModelPicker value=lab.alias entries=entries status=lab.list/>
                    <span class="lab-label">"Route"</span>
                    <TaskMenu
                        value=Signal::derive(move || {
                            if lab.editing.get() { "edit" } else { "generate" }.to_string()
                        })
                        groups=routes
                        on_pick=Callback::new(move |t: String| lab.editing.set(t == "edit"))
                    />
                    {move || {
                        lab.model()
                            .map(|m| {
                                let modes = m
                                    .modes
                                    .clone()
                                    .filter(|v| !v.is_empty())
                                    .map(|v| v.join(" · "));
                                modes.map(|s| view! { <span class="type-badge">{s}</span> })
                            })
                    }}
                    <span class="spacer"></span>
                    {move || {
                        let (cls, label) = match lab.state().as_deref() {
                            Some("ready") => ("chip ok", "container ready"),
                            Some("starting") => ("chip live", "starting…"),
                            Some("stopping") => ("chip live", "stopping…"),
                            Some(_) => ("chip off", "stopped"),
                            // A cloud alias has no container of ours.
                            None if lab.model().map(|m| m.local) == Some(false) => {
                                ("chip off", "remote")
                            }
                            None => ("chip off", "not loaded"),
                        };
                        view! {
                            <span
                                class=cls
                                title="The first render of a stopped pipeline starts its own container and uploads the weights, so it pays a cold start"
                            >
                                <i class="dot"></i>
                                {label}
                            </span>
                        }
                    }}
                    <button
                        class="btn ghost"
                        title="Reload the model list and the probed capabilities"
                        on:click=move |_| load_models()
                    >
                        "Reload"
                    </button>
                }
            }
            form=move || {
                view! {
                    <Notices lab=lab on_retry=load_models/>
                    <PresetsCard
                        lab=LabKind::Image
                        kind=LabPresetKind::ImagePrompt
                        title="Prompt presets"
                        current=preset_now
                        on_load=preset_load
                    />
                    {move || {
                        if lab.editing.get() {
                            view! { <EditPanel lab=lab/> }.into_any()
                        } else {
                            view! { <GeneratePanel lab=lab/> }.into_any()
                        }
                    }}
                    <details class="card req-card wrap" open>
                        <summary>
                            "Request — "
                            <span class="mono-sm">{move || lab.endpoint()}</span>
                        </summary>
                        <pre class="preset">{move || preview.get()}</pre>
                    </details>
                }
            }
            results=move || {
                view! {
                    {move || {
                        lab.pending
                            .get()
                            .map(|(model, endpoint, at)| {
                                view! {
                                    <div class="card result-card">
                                        <div class="run-head">
                                            <span class="run-model mono-sm">{model}</span>
                                            <span class="mono-sm dim">{endpoint}</span>
                                            <span class="spacer"></span>
                                            <span class="chip live">
                                                <i class="dot"></i>
                                                "rendering…"
                                            </span>
                                            <button class="btn danger sm" on:click=move |_| stop()>
                                                "Stop"
                                            </button>
                                            <span class="mono-sm dim">{at}</span>
                                        </div>
                                    </div>
                                }
                            })
                    }}
                    <StrayCard lab=lab/>
                    <HistoryList history=lab.history card=card/>
                }
            }
            results_n=Signal::derive(move || lab.history.total.get() as usize)
            check=check
            run_label=Signal::derive(move || {
                if lab.editing.get() { "Edit image" } else { "Generate" }.to_string()
            })
            running=lab.running
            on_run=Callback::new(move |()| run())
            on_stop=Callback::new(move |()| stop())
            on_clear=Callback::new(move |()| lab.history.clear())
            view=lab.view
            kept=Signal::derive(move || lab.history.usage.get())
            results_tools=move || view! { <ExportMenu history=lab.history models=filter_models/> }
        />
    }
}

// ---------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------

#[component]
fn Notices(lab: Lab, on_retry: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    view! {
        {move || match lab.list.get() {
            ListStatus::Failed(e) => {
                view! {
                    <div class="notice err">
                        {format!("⚠ The image model list did not load: {e}")}
                        " "
                        <button class="btn ghost sm" on:click=move |_| on_retry()>
                            "Retry"
                        </button>
                    </div>
                }
                    .into_any()
            }
            ListStatus::Ready if lab.models.with(Vec::is_empty) => {
                view! {
                    <div class="notice">
                        "No model here can draw. Add an image pipeline under "
                        <A href="/models">"Models · Image"</A>
                        " (Add from recipe is the short way), or point an alias at a cloud "
                        "image model — the picker lists whatever advertises "
                        <span class="mono-sm">"/v1/images/generations"</span> "."
                    </div>
                }
                    .into_any()
            }
            _ => ().into_any(),
        }}
        {move || {
            (lab.hold.get() && lab.model().map(|m| m.local).unwrap_or(false))
                .then(|| {
                    view! {
                        <div class="notice warn">
                            <b>"The GPU is held"</b>
                            "Local models are paused. A request either answers from this row's "
                            "fallback alias or refuses with "
                            <span class="mono-sm">"gpu_hold"</span>
                            " — release the hold from the HOLD pill at the foot of the sidebar to render here."
                        </div>
                    }
                })
        }}
        {move || {
            let w = lab.warnings();
            (!w.is_empty())
                .then(|| {
                    view! {
                        <div class="notice warn">
                            <b>"What this container's start reported"</b>
                            {w.join(" · ")}
                        </div>
                    }
                })
        }}
    }
}

// ---------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------

#[component]
fn PromptCard(lab: Lab) -> impl IntoView {
    let f = lab.form;
    view! {
        <div class="card">
            <h3 class="lab-h3">"Prompt"</h3>
            <textarea
                class="input ta"
                placeholder="What to draw…"
                prop:value=move || f.prompt.get()
                on:input=move |ev| f.prompt.set(event_target_value(&ev))
            ></textarea>
            <div class="field" style="margin-top:10px">
                <label>
                    "Negative prompt " <Fname name="negative_prompt"/>
                </label>
                <input
                    class="input"
                    placeholder="what to keep out of it"
                    prop:value=move || f.negative_prompt.get()
                    on:input=move |ev| f.negative_prompt.set(event_target_value(&ev))
                />
            </div>
            <Explain summary="Everything below the prompt travels inside it." persist="image-lab.explain.extra">
                "The OpenAI route reads only "
                <span class="mono-sm">"prompt, n, size, output_format, output_compression"</span>
                ", and sd.cpp's own "
                <span class="mono-sm">"<sd_cpp_extra_args>"</span>
                " block carries the rest. It is appended only when a field is set — see the request panel."
            </Explain>
        </div>
    }
}

#[component]
fn SizeCard(lab: Lab) -> impl IntoView {
    let f = lab.form;
    let limits = move || lab.caps().map(|c| c.limits);
    let format_opts = Signal::derive(move || {
        // The probed list when a container has answered, else the three the
        // route itself documents (§2.3) — the vocabulary, not a guess.
        let probed = lab
            .caps()
            .map(|c| c.output_formats)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                ["png", "jpeg", "webp"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
        std::iter::once((String::new(), "(server default)".to_string()))
            .chain(probed.into_iter().map(|v| (v.clone(), v)))
            .collect::<Vec<_>>()
    });
    view! {
        <div class="card">
            <h3 class="lab-h3">"Size and output"</h3>
            <div class="field-grid">
                <div class="field">
                    <label>
                        "Width " <Fname name="size"/>
                    </label>
                    <input
                        class="input"
                        type="number"
                        placeholder="server default"
                        min=move || limits().and_then(|l| l.min_width).map(|v| v.to_string())
                        max=move || limits().and_then(|l| l.max_width).map(|v| v.to_string())
                        prop:value=move || f.width.get()
                        on:input=move |ev| f.width.set(event_target_value(&ev))
                    />
                </div>
                <div class="field">
                    <label>
                        "Height " <Fname name="size"/>
                    </label>
                    <input
                        class="input"
                        type="number"
                        placeholder="server default"
                        min=move || limits().and_then(|l| l.min_height).map(|v| v.to_string())
                        max=move || limits().and_then(|l| l.max_height).map(|v| v.to_string())
                        prop:value=move || f.height.get()
                        on:input=move |ev| f.height.set(event_target_value(&ev))
                    />
                </div>
                <div class="field">
                    <label>
                        "Images " <Fname name="n"/>
                    </label>
                    <input
                        class="input"
                        type="number"
                        placeholder="server default"
                        min="1"
                        max=move || limits().and_then(|l| l.max_batch_count).map(|v| v.to_string())
                        prop:value=move || f.n.get()
                        on:input=move |ev| f.n.set(event_target_value(&ev))
                    />
                </div>
                <div class="field">
                    <label>
                        "Format " <Fname name="output_format"/>
                    </label>
                    <Select
                        value=f.output_format
                        options=format_opts
                        placeholder="(server default)"
                    />
                </div>
                <div class="field">
                    <label>
                        "Compression " <Fname name="output_compression"/>
                    </label>
                    <input
                        class="input"
                        type="number"
                        placeholder="server default"
                        prop:value=move || f.output_compression.get()
                        on:input=move |ev| f.output_compression.set(event_target_value(&ev))
                    />
                </div>
            </div>
            {move || {
                match limits() {
                    Some(l) if l.max_width.is_some() => {
                        view! {
                            <p class="dim mini-note">
                                {format!(
                                    "This pipeline accepts {}–{} × {}–{} px and up to {} image(s) per request — its own numbers, read from its capabilities.",
                                    l.min_width.unwrap_or_default(),
                                    l.max_width.unwrap_or_default(),
                                    l.min_height.unwrap_or_default(),
                                    l.max_height.unwrap_or_default(),
                                    l
                                        .max_batch_count
                                        .map(|v| v.to_string())
                                        .unwrap_or_else(|| "?".into()),
                                )}
                            </p>
                        }
                            .into_any()
                    }
                    _ => {
                        view! {
                            <p class="dim mini-note">
                                "Empty means the server's own default. No bounds are known until this model has run once and reported its capabilities — an out-of-range value comes back as the server's own refusal."
                            </p>
                        }
                            .into_any()
                    }
                }
            }}
        </div>
    }
}

#[component]
fn SamplingCard(lab: Lab) -> impl IntoView {
    let f = lab.form;
    let samplers = move || lab.caps().map(|c| c.samplers).unwrap_or_default();
    let schedulers = move || lab.caps().map(|c| c.schedulers).unwrap_or_default();
    let sampler_opts = Signal::derive(move || with_default(samplers()));
    let scheduler_opts = Signal::derive(move || with_default(schedulers()));
    view! {
        <div class="card">
            <h3 class="lab-h3">"Sampling"</h3>
            <div class="field-grid">
                <div class="field">
                    <label>
                        "Steps " <Fname name="sample_params.sample_steps"/>
                    </label>
                    <input
                        class="input"
                        type="number"
                        placeholder="server default"
                        prop:value=move || f.steps.get()
                        on:input=move |ev| f.steps.set(event_target_value(&ev))
                    />
                </div>
                <div class="field">
                    <label>
                        "CFG scale " <Fname name="sample_params.guidance.txt_cfg"/>
                    </label>
                    <input
                        class="input"
                        placeholder="server default"
                        prop:value=move || f.cfg_scale.get()
                        on:input=move |ev| f.cfg_scale.set(event_target_value(&ev))
                    />
                </div>
                <div class="field">
                    <label>
                        "Seed " <Fname name="seed"/>
                    </label>
                    <div class="seed-row">
                        <input
                            class="input"
                            placeholder="server default"
                            prop:value=move || f.seed.get()
                            on:input=move |ev| f.seed.set(event_target_value(&ev))
                        />
                        <button
                            class="btn ghost sm"
                            title="Pick a random seed to reuse; -1 means a new random seed on every run"
                            on:click=move |_| f.seed.set(random_seed().to_string())
                        >
                            "Random"
                        </button>
                    </div>
                </div>
                <div class="field">
                    <label>
                        "Sampler " <Fname name="sample_params.sample_method"/>
                    </label>
                    {move || {
                        if samplers().is_empty() {
                            view! {
                                <input
                                    class="input mono"
                                    placeholder="euler"
                                    prop:value=move || f.sampler.get()
                                    on:input=move |ev| f.sampler.set(event_target_value(&ev))
                                />
                            }
                                .into_any()
                        } else {
                            view! {
                                <Select
                                    value=f.sampler
                                    options=sampler_opts
                                    placeholder="(server default)"
                                />
                            }
                                .into_any()
                        }
                    }}
                </div>
                <div class="field">
                    <label>
                        "Scheduler " <Fname name="sample_params.scheduler"/>
                    </label>
                    {move || {
                        if schedulers().is_empty() {
                            view! {
                                <input
                                    class="input mono"
                                    placeholder="discrete"
                                    prop:value=move || f.scheduler.get()
                                    on:input=move |ev| f.scheduler.set(event_target_value(&ev))
                                />
                            }
                                .into_any()
                        } else {
                            view! {
                                <Select
                                    value=f.scheduler
                                    options=scheduler_opts
                                    placeholder="(server default)"
                                />
                            }
                                .into_any()
                        }
                    }}
                </div>
            </div>
            <Show when=move || samplers().is_empty()>
                <p class="dim mini-note">
                    "Start the model to load its sampler list — the names come from the running container's own capabilities, and a typed one is passed through as written."
                </p>
            </Show>
        </div>
    }
}

/// `(value, label)` options with the "say nothing" entry in front.
fn with_default(values: Vec<String>) -> Vec<(String, String)> {
    std::iter::once((String::new(), "(server default)".to_string()))
        .chain(values.into_iter().map(|v| (v.clone(), v)))
        .collect()
}

#[component]
fn LoraCard(lab: Lab) -> impl IntoView {
    let f = lab.form;
    let known = move || {
        lab.caps()
            .map(|c| c.loras.into_iter().map(|a| a.name).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let add = move |_| {
        let id = f.next_lora.get_untracked();
        f.next_lora.set(id + 1);
        f.loras
            .update(|v| v.push((id, String::new(), String::new())));
    };
    view! {
        <div class="card">
            <h3 class="lab-h3">"LoRA"</h3>
            <For each=move || f.loras.get() key=|(id, _, _)| *id let:row>
                {
                    let (id, path, mult) = row;
                    let known_opts = Signal::derive(move || with_default(known()));
                    let pick = RwSignal::new(path.clone());
                    // The picker writes the path field; a name that is not on
                    // disk here (a cloud model's, a fresh file) is still
                    // typeable, because the server resolves it, not this page.
                    Effect::new(move |_| {
                        let v = pick.get();
                        if !v.is_empty() {
                            f.loras
                                .update(|rows| {
                                    if let Some(r) = rows.iter_mut().find(|r| r.0 == id) {
                                        r.1 = v.clone();
                                    }
                                });
                        }
                    });
                    view! {
                        <div class="lora-row">
                            <input
                                class="input mono"
                                placeholder="lora name or path under --lora-model-dir"
                                prop:value=path
                                on:input=move |ev| {
                                    let v = event_target_value(&ev);
                                    f.loras
                                        .update(|rows| {
                                            if let Some(r) = rows.iter_mut().find(|r| r.0 == id) {
                                                r.1 = v.clone();
                                            }
                                        });
                                }
                            />
                            // Always a cell, so the row's four columns line
                            // up whether or not this model has LoRAs on disk.
                            <div class="lora-pick">
                                <Show when=move || !known().is_empty()>
                                    <Select value=pick options=known_opts placeholder="on disk"/>
                                </Show>
                            </div>
                            <input
                                class="input"
                                placeholder="multiplier"
                                prop:value=mult
                                on:input=move |ev| {
                                    let v = event_target_value(&ev);
                                    f.loras
                                        .update(|rows| {
                                            if let Some(r) = rows.iter_mut().find(|r| r.0 == id) {
                                                r.2 = v.clone();
                                            }
                                        });
                                }
                            />
                            <button
                                class="btn ghost del"
                                title="Remove this LoRA"
                                on:click=move |_| f.loras.update(|rows| rows.retain(|r| r.0 != id))
                            >
                                "✕"
                            </button>
                        </div>
                    }
                }
            </For>
            <div class="row" style="margin-top:8px">
                <button class="btn ghost" on:click=add>
                    "Add LoRA"
                </button>
                {move || {
                    let n = known().len();
                    (n > 0)
                        .then(|| {
                            view! {
                                <span class="dim mono-sm">
                                    {format!("{n} found under --lora-model-dir")}
                                </span>
                            }
                        })
                }}
            </div>
            <Explain summary="A LoRA is a field of the request, never a prompt tag." persist="image-lab.explain.lora">
                "It is the structured " <span class="mono-sm">"lora"</span>
                " field inside the extension block — "
                <span class="mono-sm">"<lora:name:1.0>"</span>
                " prompt tags are refused by every sd.cpp family."
            </Explain>
        </div>
    }
}

#[component]
fn GeneratePanel(lab: Lab) -> impl IntoView {
    view! {
        <PromptCard lab=lab/>
        <SizeCard lab=lab/>
        <SamplingCard lab=lab/>
        <LoraCard lab=lab/>
    }
}

#[component]
fn EditPanel(lab: Lab) -> impl IntoView {
    let f = lab.form;
    view! {
        <div class="card">
            <h3 class="lab-h3">"Reference image"</h3>
            <div class="field-grid">
                <div class="field">
                    <label>
                        "Image " <Fname name="image"/>
                    </label>
                    <input
                        class="input file"
                        type="file"
                        accept="image/*"
                        on:change=move |ev| {
                            let el: web_sys::HtmlInputElement = event_target(&ev);
                            let file = el.files().and_then(|l| l.get(0));
                            f.image_name.set(file.as_ref().map(|x| x.name()).unwrap_or_default());
                            f.image_ref.set(None);
                            f.image_file.set_value(file);
                        }
                    />
                </div>
                <div class="field">
                    <label>
                        "Mask (optional) " <Fname name="mask"/>
                    </label>
                    <input
                        class="input file"
                        type="file"
                        accept="image/*"
                        on:change=move |ev| {
                            let el: web_sys::HtmlInputElement = event_target(&ev);
                            let file = el.files().and_then(|l| l.get(0));
                            f.mask_name.set(file.as_ref().map(|x| x.name()).unwrap_or_default());
                            f.mask_ref.set(None);
                            f.mask_file.set_value(file);
                        }
                    />
                </div>
            </div>
            {move || {
                let name = f.image_name.get();
                (!name.is_empty())
                    .then(|| {
                        view! {
                            <p class="dim mono-sm">
                                {format!(
                                    "{name}{}",
                                    if f.mask_name.get().is_empty() {
                                        String::new()
                                    } else {
                                        format!(" · mask {}", f.mask_name.get())
                                    },
                                )}
                            </p>
                        }
                    })
            }}
            {move || {
                let kept: Vec<String> = [f.image_ref.get(), f.mask_ref.get()]
                    .into_iter()
                    .flatten()
                    .map(|r| r.name)
                    .collect();
                (!kept.is_empty())
                    .then(|| {
                        view! {
                            <p class="mini-note il-kept">
                                {format!("kept: {} — sent from the lab store, no upload. ", kept.join(" · "))}
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    title="Forget the kept pictures; pick files instead"
                                    on:click=move |_| {
                                        f.image_ref.set(None);
                                        f.mask_ref.set(None);
                                        f.image_name.set(String::new());
                                        f.mask_name.set(String::new());
                                    }
                                >
                                    "Clear"
                                </button>
                            </p>
                        }
                    })
            }}
            <p class="dim mini-note">
                "The upload is relayed as multipart through "
                <span class="mono-sm">"/v1/images/edits"</span>
                " — the same path an OpenAI images client takes. The prompt, size and batch below are the edit's."
            </p>
        </div>
        <PromptCard lab=lab/>
        <SizeCard lab=lab/>
        <SamplingCard lab=lab/>
    }
}

// ---------------------------------------------------------------------------
// Side panel — what the loaded pipeline says about itself
// ---------------------------------------------------------------------------

#[component]
fn SidePanel(lab: Lab) -> impl IntoView {
    view! {
        <div class="lab-help">
            {move || {
                let Some(m) = lab.model() else {
                    return view! { <div class="empty">"No model selected."</div> }.into_any();
                };
                let caps = lab.caps();
                view! {
                    <p class="dim mini-note">
                        "Routes: "
                        <span class="mono-sm">{m.endpoints.join(", ")}</span>
                    </p>
                    // Folded: these are the same notes `/v1/models` publishes
                    // for this id — five paragraphs of contract, worth reading
                    // once and not worth burying the numbers below them.
                    {(!m.notes.is_empty())
                        .then(|| {
                            let n = m.notes.len();
                            view! {
                                <details class="lab-details">
                                    <summary>{format!("What it publishes — {n}")}</summary>
                                    <ul class="side-notes">
                                        {m
                                            .notes
                                            .iter()
                                            .map(|n| view! { <li>{n.clone()}</li> })
                                            .collect::<Vec<_>>()}
                                    </ul>
                                </details>
                            }
                        })}
                    {match caps {
                        None => {
                            view! {
                                <p class="dim mini-note">
                                    {if m.local {
                                        "This container has not reported its capabilities yet — it is not running, or its probe did not answer. The sampler and scheduler fields are free text until it does."
                                    } else {
                                        "A cloud model publishes no sampler list; everything in the extension block is passed through to the provider as written."
                                    }}
                                </p>
                            }
                                .into_any()
                        }
                        Some(c) => {
                            view! {
                                <div class="caps">
                                    <div class="caps-row">
                                        <span class="k">"modes"</span>
                                        <span class="v mono-sm">{c.supported_modes.join(", ")}</span>
                                    </div>
                                    <div class="caps-row">
                                        <span class="k">"samplers"</span>
                                        <span class="v mono-sm">{c.samplers.len()}</span>
                                    </div>
                                    <div class="caps-row">
                                        <span class="k">"schedulers"</span>
                                        <span class="v mono-sm">{c.schedulers.len()}</span>
                                    </div>
                                    <div class="caps-row">
                                        <span class="k">"formats"</span>
                                        <span class="v mono-sm">{c.output_formats.join(", ")}</span>
                                    </div>
                                    <div class="caps-row">
                                        <span class="k">"loras"</span>
                                        <span class="v mono-sm">{c.loras.len()}</span>
                                    </div>
                                    <div class="caps-row">
                                        <span class="k">"upscalers"</span>
                                        <span class="v mono-sm">{c.upscalers.len()}</span>
                                    </div>
                                    // What the pipeline *claims*, which is not
                                    // the edit gate: Z-Image-Turbo answers yes
                                    // here and then dies on a reference image
                                    // (§12.8), so the row's column decides.
                                    <div
                                        class="caps-row"
                                        title="The pipeline's own claim. The row's `edit` column is what decides whether /v1/images/edits is served — a pipeline that cannot take a reference image does not refuse one, it crashes."
                                    >
                                        <span class="k">"reports ref images"</span>
                                        <span class="v mono-sm">
                                            {match (c.features.ref_images, m.edit) {
                                                (true, false) => "yes — not the gate",
                                                (true, true) => "yes",
                                                _ => "no",
                                            }}
                                        </span>
                                    </div>
                                </div>
                            }
                                .into_any()
                        }
                    }}
                }
                    .into_any()
            }}
            {move || {
                (!lab.can_edit())
                    .then(|| {
                        let local = lab.model().map(|m| m.local).unwrap_or(false);
                        view! {
                            <div class="notice">
                                <b>"No edit panel for this model"</b>
                                {if local {
                                    "Its row has edit = false, so the pipeline takes no reference image. sd-server does not refuse such a request — it crashes on it — so lmgw refuses it first. Set `edit` on the row (Models · Image) only for a real edit pipeline: Kontext, Qwen-Image-Edit, Z-Image-Omni."
                                } else {
                                    "Its upstream catalog does not list `image` among the model's input modalities, so /v1/images/edits is not offered for it."
                                }}
                            </div>
                        }
                    })
            }}
            <div class="row" style="margin-top:auto">
                {move || {
                    let dir = lab.models_dir.get();
                    (!dir.is_empty())
                        .then(|| view! { <p class="dim mono-sm path-note">{dir}</p> })
                }}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies_render_their_message_and_code() {
        assert_eq!(
            error_parts(
                r#"{"error": {"message": "model not served", "code": "unsupported"}}"#,
                400
            ),
            ("model not served".to_string(), "unsupported".to_string())
        );
        assert_eq!(
            error_parts(r#"{"error": "prompt required"}"#, 400).0,
            "prompt required"
        );
        assert_eq!(error_parts("", 503).0, "HTTP 503");
    }

    /// A row's own flags are the form's defaults; anything else stays empty,
    /// which is how the server gets to use its default.
    #[test]
    fn row_args_prefill_only_what_they_state() {
        let args = serde_json::json!({"width": 1024, "cfg_scale": "1.0", "diffusion_fa": true});
        let args = Some(args.as_object().unwrap().clone());
        assert_eq!(arg_text(&args, "width"), "1024");
        assert_eq!(arg_text(&args, "cfg_scale"), "1.0");
        assert_eq!(arg_text(&args, "steps"), "");
        assert_eq!(arg_text(&args, "diffusion_fa"), "");
        assert_eq!(arg_text(&None, "width"), "");
    }
}
