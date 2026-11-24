//! Models: everything this gateway is set up to serve — the four local
//! runtimes and the cloud aliases — as a list beside the selected row's
//! detail (docs/design/ui-rebuild/modern-sample.html), plus one row per
//! passthrough upstream pointing at its catalog (`model_catalog.rs`), which
//! is where the hundreds of models an upstream like kilo offers live.
//!
//! The selection is the path (`/models/<class>/<id>`, [`route`]), so a link
//! from another page opens the same view with that row selected, and leaving
//! a row with unsaved edits asks through the dirty guard. This module keeps
//! what the panes share: the list's data, its rows, and the ops a row runs.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{
    AliasView, AudioModel, AuxModel, CandidateAliasView, ImageModel, ModelTest, ModelsFull,
    OpOutcome, RuntimeStatus,
};
use serde_json::{json, Value};

mod aux_row;
/// The detail pane: the selected row's header actions, hero and settings.
mod detail;
/// `/models/find`: Find a model by its name (find-a-model design §8.1).
mod find;
mod licence;
/// The list pane: the header, the filter, the search and the rows.
mod list;
/// Which row the path selects.
mod route;
/// The list beside the detail, the handle between them, and the narrow
/// window's one-pane navigation.
mod split;
pub use find::ModelFindPage;

use crate::catalog::use_model_catalog;
use crate::fmt::fmt_bytes;
use crate::model_ops::{container_action, LogsModal, StopRefusedModal};
use crate::ops_state::{use_ops, OpsState};
use crate::widgets::list_pane::Nav;
use crate::widgets::{use_toasts, Modal, ModalFooter, Toasts};

/// Bumped after every mutation: the page refetches its list into place and
/// refreshes the shared model catalog. Kept as a context so the wizard and
/// the editors' callers report a change without knowing what depends on it.
#[derive(Clone, Copy)]
pub struct ModelsReload(pub RwSignal<u32>);

/// Page-level state of the audio spec-catalog browser: the group row opens
/// it, the single modal at the bottom of the page renders it.
#[derive(Clone, Copy)]
pub struct CatalogOpen(pub RwSignal<bool>);

/// Same, for the image class's "Add from recipe" browser
/// (image-generation §7.2). A separate signal rather than an enum because the
/// two browsers are independent surfaces that happen to sit on one page.
#[derive(Clone, Copy)]
pub struct RecipesOpen(pub RwSignal<bool>);

/// Page-level edit-modal state; rows write, the single modal pair reads.
#[derive(Clone, Copy)]
pub struct Editors {
    pub alias: RwSignal<Option<AliasView>>,
    pub aux: RwSignal<Option<AuxModel>>,
    pub audio: RwSignal<Option<AudioModel>>,
    /// The image editor's seed. The recipe browser writes a row it prefilled
    /// from `ImageRecipeAddResult.row` here, which is the whole handover
    /// between the two surfaces — the same path the audio catalog uses.
    pub image: RwSignal<Option<ImageModel>>,
    /// The prompt style the matching recipe gives the row the image editor
    /// opened on (`ImageModelView::derived_prompt_style`); none for a blank
    /// or a recipe's prefilled row, whose match the save makes.
    pub image_derived: RwSignal<Option<lmgw_api_types::chat_media::PromptStyle>>,
    pub candidate: RwSignal<Option<CandidateAliasView>>,
}

/// A blank audio model (id 0) opens `AudioEditor` in "create" mode — the
/// manual entry point audio needs since, unlike aux models, there's no
/// wizard auto-create for it (family/task/voice_presets aren't derivable
/// from a downloaded GGUF alone). The catalog browser starts from the same
/// blank and fills in what the spec package knows.
pub(super) fn blank_audio_model() -> AudioModel {
    AudioModel {
        id: 0,
        model_id: String::new(),
        family: String::new(),
        path: String::new(),
        task: "tts".into(),
        mode: "offline".into(),
        lazy: None,
        busy_timeout_ms: None,
        // Inherit the class: a new row runs where the class does.
        backend: None,
        threads: None,
        load_options: Default::default(),
        session_options: Default::default(),
        default_request_options: Default::default(),
        model_spec_override: None,
        config_id: None,
        weight_id: None,
        voice_presets: Default::default(),
        default_voice_preset: None,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        fallback: None,
        // Learned by the gateway, never by the form.
        residency: None,
    }
}

/// A blank image model (id 0) opens `ImageEditor` in "create" mode. The
/// defaults are the row defaults `ops::image_model_set` applies when the patch
/// leaves them out (`idle_seconds` 300, `enabled`, no fallback), so an
/// untouched form saves as the same row the ops plane would
/// have written — the form states them rather than hiding them.
pub(super) fn blank_image_model() -> ImageModel {
    ImageModel {
        id: 0,
        model_id: String::new(),
        files: Default::default(),
        args: Default::default(),
        modes: Vec::new(),
        edit: false,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        idle_seconds: 300,
        fallback: None,
        capabilities_override: None,
        // Learned by the gateway, never by the form: a row that has never run
        // has nothing to say about what a generation costs.
        peak_extra_bytes: None,
        peak_learned_at: None,
        idle_bytes: None,
        work_bytes: None,
        learned_key: None,
        // The editor has no fields for these yet: unset derives the prompt
        // style from the recipe and keeps the default preview.
        prompt_style: None,
        preview_mode: None,
    }
}

/// A blank aux model (id 0) opens `AuxEditor` in "create" mode, with the
/// defaults `ops::aux_model_set` applies to a create that leaves them out.
/// The wizard's aux target is the usual way in; this is the one for a GGUF
/// that is already in the aux models dir.
fn blank_aux_model() -> AuxModel {
    AuxModel {
        id: 0,
        model_id: String::new(),
        gguf_path: String::new(),
        kind: "embed".into(),
        pooling: None,
        ctx_size: None,
        ubatch_size: None,
        batch_size: None,
        mmproj_path: None,
        backend: None,
        threads: None,
        args: Vec::new(),
        idle_seconds: 300,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        fallback: None,
    }
}

pub fn bump(reload: ModelsReload) {
    reload.0.update(|n| *n += 1);
}

/// `/api/models/full`, held where ops can refresh it in place: a refetch
/// lands *in* this signal and the table under it updates row by row. Never a
/// `match` over a resource that rebuilds the page — that is what used to
/// throw away the scroll position on every Hide.
#[derive(Clone, Copy)]
pub(super) struct ModelsData {
    pub full: RwSignal<Option<ModelsFull>>,
    /// The last fetch's failure. The list keeps what the one before got.
    pub error: RwSignal<Option<String>>,
    /// Which fetch is the latest: an older one landing late is dropped.
    generation: StoredValue<u64>,
}

impl ModelsData {
    pub fn new() -> Self {
        let data = Self {
            full: RwSignal::new(None),
            error: RwSignal::new(None),
            generation: StoredValue::new(0),
        };
        data.load();
        data
    }

    pub fn load(&self) {
        let this = *self;
        // Called from an op's continuation too, which can land after the
        // page is gone: then there is nothing to load into.
        let Some(gen) = this.generation.try_get_value().map(|g| g + 1) else {
            return;
        };
        this.generation.set_value(gen);
        spawn_local(async move {
            let res = crate::api::get::<ModelsFull>("/api/models/full").await;
            if this.generation.try_get_value() != Some(gen) {
                return;
            }
            match res {
                Ok(m) => {
                    this.full.set(Some(m));
                    this.error.set(None);
                }
                Err(e) => this.error.set(Some(e.to_string())),
            }
        });
    }
}

/// The configured classes, in table order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Class {
    Chat,
    Aux,
    Audio,
    Image,
    Alias,
    /// Candidate aliases (candidate-aliases design §4.1, §6): a client-facing
    /// name backed by a primary local chat model plus alternates, never a
    /// container of its own — the underlying candidates' rows carry the
    /// runtime.
    Candidate,
}

impl Class {
    const ALL: [Self; 6] = [
        Self::Chat,
        Self::Aux,
        Self::Audio,
        Self::Image,
        Self::Alias,
        Self::Candidate,
    ];

    /// The filter's id for the class (`?show=`).
    fn id(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Aux => "embed",
            Self::Audio => "audio",
            Self::Image => "image",
            Self::Alias => "aliases",
            Self::Candidate => "candidates",
        }
    }

    /// What `live.runtime` and the container op call the class. An alias
    /// has no container — a candidate alias either, even though the
    /// candidates it points at do (its state is what its own derivation
    /// last found: a problem count).
    fn runtime(self) -> Option<&'static str> {
        match self {
            Self::Chat => Some("chat"),
            Self::Aux => Some("aux"),
            Self::Audio => Some("audio"),
            Self::Image => Some("image"),
            Self::Alias | Self::Candidate => None,
        }
    }

    /// The op that enables, disables and deletes a row of the class.
    fn op(self) -> &'static str {
        match self {
            Self::Chat => "local_model_set",
            Self::Aux => "aux_model_set",
            Self::Audio => "audio_model_set",
            Self::Image => "image_model_set",
            Self::Alias => "model_set",
            Self::Candidate => "candidate_alias_set",
        }
    }

    /// What `local_model_test` does for the class, for the Test tooltip.
    /// Audio models are exercised in the Audio lab; aliases have no container.
    /// An aux row's own kind says which probe it gets
    /// ([`aux_row::probe_of`]).
    fn probe(self) -> Option<&'static str> {
        match self {
            Self::Chat => Some("generates one token"),
            Self::Aux => Some("embeds, reranks or asks a decision question"),
            Self::Image => Some("draws one small image"),
            Self::Audio | Self::Alias | Self::Candidate => None,
        }
    }

    /// The group's line while it holds nothing, with the way to fill it.
    fn empty(self) -> &'static str {
        match self {
            Self::Chat => "none yet — add one from Hugging Face or from disk",
            Self::Aux => "none yet — the Hugging Face wizard's aux target makes one",
            Self::Audio => "none yet — add one from the catalog",
            Self::Image => "none yet — add one from a recipe",
            Self::Alias => {
                "none yet — map a friendly name to an upstream model, or use Alias… in a catalog"
            }
            Self::Candidate => {
                "none yet — pick a primary local model that already covers the facets you need"
            }
        }
    }
}

/// One configured model, flattened for the table: every class fills the same
/// cells, so the columns line up across groups.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Row {
    class: Class,
    id: i64,
    /// The class's own id: what the runtime frame, the load test and the
    /// container ops name it by.
    model_id: String,
    /// What a client requests.
    name: String,
    kind: String,
    /// More about the kind, for the badge's tooltip (pooling, family, mode).
    kind_title: String,
    /// A short flag after the type badge ("alias only", "edits"): what
    /// changes how the model can be reached or used.
    note: String,
    file: String,
    /// Everything behind `file` (full paths, companion files), for its tooltip.
    file_title: String,
    /// What the copy button in the file cell copies.
    copy: String,
    ctx: Option<i64>,
    enabled: bool,
    /// A candidate alias's own problem count (§4.6) — `CandidateStateCell`'s
    /// State-cell chip; always 0 for every other class.
    problems: usize,
    /// The `gpu_deferred` count over the last 24 h of requests for this
    /// row's name: a candidate alias's (§6), a local model's background
    /// requests (per-request background design §10); `None` for a plain
    /// alias, which always routes to a stored upstream — a provider, or a
    /// server lmgw does not start — so lmgw has nothing of its own to defer.
    deferrals_24h: Option<u64>,
    /// The licence the row's GGUF states, for the chip beside the kind;
    /// `None` for a row whose header says nothing (and for every class
    /// without a GGUF header to read).
    licence: Option<Box<licence::Chip>>,
    /// Its one fallback (openai egress design §14.12): what answers when it
    /// cannot, so "which local models would refuse under the hold" is one
    /// look down the column.
    fallback: Option<String>,
}

impl Row {
    fn matches(&self, words: &[String]) -> bool {
        if words.is_empty() {
            return true;
        }
        let hay = format!(
            "{} {} {} {} {} {}",
            self.name,
            self.model_id,
            self.kind_title,
            self.note,
            self.file_title,
            self.licence.as_deref().map_or("", licence::Chip::haystack),
        )
        .to_lowercase();
        words.iter().all(|w| hay.contains(w.as_str()))
    }
}

fn file_name(path: &str) -> String {
    let p = path.trim_end_matches('/');
    p.rsplit('/').next().unwrap_or(p).to_string()
}

/// An audio model is a directory, often a variant of a package
/// (`…/PocketTTS-GGUF/english`): a short last segment keeps its parent, so
/// the row says which package it is.
fn short_dir(path: &str) -> String {
    let p = path.trim_end_matches('/');
    let mut parts = p.rsplit('/');
    match (parts.next(), parts.next()) {
        (Some(last), Some(parent)) if last.len() < 12 => format!("{parent}/{last}"),
        (Some(last), _) => last.to_string(),
        _ => p.to_string(),
    }
}

fn rows_of(m: &ModelsFull) -> Vec<Row> {
    let mut out = Vec::new();
    for v in &m.local {
        let l = &v.model;
        let mut title = l.gguf_path.clone();
        if let Some(p) = &l.params.mmproj_path {
            title.push_str(&format!("\nprojector: {p}"));
        }
        if let Some(p) = &l.params.draft_gguf_path {
            title.push_str(&format!("\ndrafter: {p}"));
        }
        out.push(Row {
            class: Class::Chat,
            id: l.id,
            model_id: l.model_id.clone(),
            name: v.public_name.clone(),
            kind: "chat".into(),
            kind_title: if l.public {
                "chat · listed in /v1/models".into()
            } else {
                "chat · not listed in /v1/models: reachable only through an alias".into()
            },
            note: if l.public {
                String::new()
            } else {
                "alias only".into()
            },
            file: file_name(&l.gguf_path),
            file_title: title,
            copy: l.gguf_path.clone(),
            ctx: l.params.ctx_size,
            enabled: l.enabled,
            problems: 0,
            deferrals_24h: Some(v.deferrals_24h),
            licence: v.licence.as_ref().and_then(licence::Chip::of).map(Box::new),
            fallback: l.fallback.clone(),
        });
    }
    for v in &m.aux {
        let a = &v.model;
        out.push(Row {
            class: Class::Aux,
            id: a.id,
            model_id: a.model_id.clone(),
            name: v.public_name.clone(),
            kind: a.kind.clone(),
            kind_title: match (a.kind.as_str(), &a.pooling) {
                ("decision", _) => "decision · answers /v1/systemone".into(),
                ("rerank", _) => "rerank · answers /v1/rerank".into(),
                (_, Some(p)) => format!("{} · pooling {p}", a.kind),
                (_, None) => format!("{} · pooling from the GGUF", a.kind),
            },
            note: String::new(),
            file: aux_row::file(v),
            file_title: aux_row::file_title(v),
            copy: a.gguf_path.clone(),
            ctx: a.ctx_size,
            enabled: a.enabled,
            problems: 0,
            deferrals_24h: Some(v.deferrals_24h),
            licence: v.licence.as_ref().and_then(licence::Chip::of).map(Box::new),
            fallback: a.fallback.clone(),
        });
    }
    for v in &m.audio {
        let a = &v.model;
        // What its container was measured to hold once loaded (realtime
        // design §9.4) — the figure admission charges, or why it charges the
        // on-disk size instead, which the tooltip says. A row on the CPU
        // holds nothing on the GPU: its thread count instead.
        let mut file = short_dir(&a.path);
        match (v.runs_on.as_str(), v.residency_charged_bytes, &a.residency) {
            ("cpu", _, _) => file.push_str(&format!(" · CPU · {} threads", v.threads_in_effect)),
            (_, Some(b), _) => file.push_str(&format!(" · resident {}", fmt_bytes(b))),
            (_, None, Some(r)) => file.push_str(&format!(
                " · resident {}, previous config",
                fmt_bytes(r.bytes)
            )),
            (_, None, None) => {}
        }
        let mut file_title = a.path.clone();
        if let Some(note) = &v.residency_note {
            file_title.push('\n');
            file_title.push_str(note);
        }
        out.push(Row {
            class: Class::Audio,
            id: a.id,
            model_id: a.model_id.clone(),
            name: v.public_name.clone(),
            kind: super::audio_spec::task_label(&a.task),
            kind_title: format!("{} · family {} · {} mode", a.task, a.family, a.mode),
            note: String::new(),
            file,
            file_title,
            copy: a.path.clone(),
            ctx: None,
            enabled: a.enabled,
            problems: 0,
            deferrals_24h: Some(v.deferrals_24h),
            licence: None,
            fallback: a.fallback.clone(),
        });
    }
    for v in &m.image {
        let i = &v.model;
        // The pipeline in one line: whichever of the two exclusive entry
        // points it names, plus how many other files hang off it.
        let primary = ["model", "diffusion_model"]
            .iter()
            .find_map(|k| i.files.get(*k).and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let others = i
            .files
            .len()
            .saturating_sub(usize::from(!primary.is_empty()));
        let mut file = if primary.is_empty() {
            "no checkpoint or diffusion model".to_string()
        } else {
            file_name(&primary)
        };
        if others > 0 {
            file.push_str(&format!(" + {others} files"));
        }
        let mut title: Vec<String> = i
            .files
            .iter()
            .map(|(role, p)| format!("{role}: {}", p.as_str().unwrap_or_default()))
            .collect();
        // What one generation was measured to need on top of the idle
        // residency, learned by the gateway (image-generation §9).
        if let Some(p) = i.peak_extra_bytes {
            file.push_str(&format!(" · peak +{}", fmt_bytes(p)));
            title.push(format!(
                "peak +{} while generating: admission keeps this much free on top of the \
                 idle residency; a files or args change resets it",
                fmt_bytes(p)
            ));
        }
        out.push(Row {
            class: Class::Image,
            id: i.id,
            model_id: i.model_id.clone(),
            name: v.public_name.clone(),
            kind: if i.modes.is_empty() {
                crate::labels::image_mode_label("img_gen")
            } else {
                i.modes
                    .iter()
                    .map(|m| crate::labels::image_mode_label(m))
                    .collect::<Vec<_>>()
                    .join(" · ")
            },
            kind_title: "stable-diffusion.cpp pipeline".into(),
            note: if i.edit {
                "Edits images".into()
            } else {
                String::new()
            },
            file,
            file_title: title.join("\n"),
            copy: primary,
            ctx: None,
            enabled: i.enabled,
            problems: 0,
            deferrals_24h: Some(v.deferrals_24h),
            licence: None,
            fallback: i.fallback.clone(),
        });
    }
    for a in &m.aliases {
        let o = &a.param_overrides;
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = o.temperature {
            parts.push(format!("temp {t}"));
        }
        if let Some(m) = o.max_tokens {
            parts.push(format!("max {m}"));
        }
        if let Some(p) = o.top_p {
            parts.push(format!("top_p {p}"));
        }
        let upstream = a
            .upstream_name
            .clone()
            .unwrap_or_else(|| format!("upstream #{}", a.upstream_id));
        out.push(Row {
            class: Class::Alias,
            id: a.id,
            model_id: a.alias.clone(),
            name: a.alias.clone(),
            kind: "alias".into(),
            kind_title: if parts.is_empty() {
                "alias · no parameter overrides".into()
            } else {
                format!("alias · {}", parts.join(" · "))
            },
            note: String::new(),
            file: format!("{upstream} · {}", a.upstream_model_id),
            file_title: format!("{upstream} · {}", a.upstream_model_id),
            copy: a.upstream_model_id.clone(),
            ctx: None,
            enabled: a.enabled,
            problems: 0,
            deferrals_24h: None,
            licence: None,
            fallback: a.fallback.clone(),
        });
    }
    for c in &m.candidate_aliases {
        let (primary, alternates) = c
            .candidates
            .split_first()
            .map(|(p, alts)| (p.clone(), alts.to_vec()))
            .unwrap_or_default();
        let file = if alternates.is_empty() {
            primary.clone()
        } else {
            format!("{primary} → {}", alternates.join(", "))
        };
        let mut kind_title = if c.background {
            "candidate alias · background: never evicts or interrupts the owner's models".into()
        } else {
            "candidate alias".to_string()
        };
        if !c.enabled_facets.is_empty() {
            kind_title.push_str(&format!(" · enables {}", c.enabled_facets.join(", ")));
        }
        out.push(Row {
            class: Class::Candidate,
            id: c.id,
            model_id: c.alias.clone(),
            name: c.alias.clone(),
            kind: "candidate".into(),
            kind_title,
            note: if c.background {
                "background".into()
            } else {
                String::new()
            },
            file,
            file_title: format!(
                "primary: {primary}\nalternates: {}",
                if alternates.is_empty() {
                    "none".to_string()
                } else {
                    alternates.join(", ")
                }
            ),
            copy: primary,
            ctx: c.context_length.and_then(|v| i64::try_from(v).ok()),
            enabled: c.enabled,
            problems: c.problems.len(),
            deferrals_24h: c.deferrals_24h,
            licence: None,
            fallback: c.fallback.clone(),
        });
    }
    out
}

fn runtime_of<'a>(
    rt: &'a [RuntimeStatus],
    class: Class,
    model_id: &str,
) -> Option<&'a RuntimeStatus> {
    let c = class.runtime()?;
    rt.iter().find(|r| r.class == c && r.model_id == model_id)
}

fn is_running(rt: &[RuntimeStatus], r: &Row) -> bool {
    runtime_of(rt, r.class, &r.model_id).is_some_and(|x| x.state != "stopping")
}

/// One passthrough upstream's line in the "Upstream catalogs" group.
#[derive(Clone, PartialEq, Eq, Hash)]
struct CatalogLine {
    id: i64,
    name: String,
    prefix: String,
    hidden: usize,
}

/// What a row reaches: every op goes through here, so each one refetches
/// the list and refreshes the shared catalog the same way. Captured at
/// render — event handlers run with no reactive owner to look context up in.
#[derive(Clone, Copy)]
pub(super) struct PageCtx {
    toasts: Toasts,
    reload: ModelsReload,
    editors: Editors,
    data: ModelsData,
    ops: OpsState,
    logs: RwSignal<Option<(&'static str, String)>>,
    /// A stop the gateway refused, (class, model, message), for the forced
    /// stop offer.
    refused: RwSignal<Option<(&'static str, String, String)>>,
    /// The row whose Delete… asks, (class, id, name).
    deleting: RwSignal<Option<(Class, i64, String)>>,
    /// The page's moves, the unsaved-edits guard asked first.
    nav: Nav,
    /// The Hugging Face wizard's modal.
    wizard_open: RwSignal<bool>,
}

impl PageCtx {
    /// Fire a mutation op, toast the outcome, refetch. Container hygiene
    /// (stopping a container whose config just went stale) is the backend's
    /// job — `local_model_set`/`aux_model_set`/`audio_model_set` already do it
    /// on every update/enable/disable.
    fn op(self, name: &'static str, args: Value) {
        spawn_local(async move {
            match crate::api::post::<OpOutcome, _>(format!("/api/op/{name}"), &args).await {
                Ok(v) => {
                    // Every class's save answers `message`; they share nothing
                    // else (`AuxModelSaved`, `CandidateAliasSaved`, ...).
                    self.toasts.ok(v.message.unwrap_or_else(|| "done".into()));
                    // A delete names the rows it left without a working
                    // fallback ("confirm, then do"): each as a warning.
                    for note in v.fallback_notes {
                        self.toasts.warn(note);
                    }
                    bump(self.reload);
                }
                Err(e) => self.toasts.err(e.to_string()),
            }
        });
    }

    /// Go to `to` (a path in the app), asking first when unsaved edits
    /// would be left behind ([`Nav::go`]). Whether it moved now.
    fn go(self, to: &str, replace: bool) -> bool {
        self.nav.go(to, replace)
    }

    /// Go to `to` without asking: what would be asked about is gone already
    /// (the edits of a row that was deleted).
    fn go_unasked(self, to: &str, replace: bool) {
        self.nav.go_unasked(to, replace);
    }

    fn container(self, class: &'static str, model_id: String, action: &'static str) {
        let refused = self.refused;
        let mid = model_id.clone();
        container_action(
            self.ops,
            self.toasts,
            class,
            model_id,
            action,
            false,
            move |msg| refused.set(Some((class, mid, msg))),
        );
    }
}

/// `local_model_test` for one row, toasting what it found. A test that
/// reached the model but failed answers 200 with `ok: false`, the error and
/// a hint — that is bad news too, not a success.
fn run_test(toasts: Toasts, class: Class, model_id: String, testing: RwSignal<bool>) {
    if testing.get_untracked() {
        return;
    }
    testing.set(true);
    let mut body = json!({ "model_id": model_id.clone() });
    match class {
        Class::Aux => body["target"] = json!("aux"),
        Class::Image => body["target"] = json!("image"),
        _ => {}
    }
    spawn_local(async move {
        let res = crate::api::post::<ModelTest, _>("/api/op/local_model_test", &body).await;
        testing.set(false);
        let found = match res {
            Ok(found) => found,
            Err(e) => return toasts.err(e.to_string()),
        };
        let ms = |latency_ms: u64| format!(" in {latency_ms} ms");
        match found {
            ModelTest::Failed(failed) => toasts.err(if failed.hint.is_empty() {
                failed.error
            } else {
                format!("{} — {}", failed.error, failed.hint)
            }),
            ModelTest::Ladder(ladder) if !ladder.ok => toasts.err("load test failed"),
            ModelTest::Ladder(_) => toasts.ok(format!("{model_id}: loads and generates")),
            ModelTest::Passed(passed) => toasts.ok(aux_row::passed(&model_id, &passed)),
            ModelTest::ImagePassed(drew) => toasts.ok(format!(
                "{model_id}: drew {}{} — {}",
                drew.size,
                ms(drew.latency_ms),
                fmt_bytes(drew.bytes as u64)
            )),
            ModelTest::ChatPassed(passed) => toasts.ok(format!(
                "{model_id}: loads and generates{}",
                ms(passed.base.latency_ms)
            )),
        }
    });
}

#[component]
pub fn Models() -> impl IntoView {
    let reload = ModelsReload(RwSignal::new(0));
    provide_context(reload);
    let editors = Editors {
        alias: RwSignal::new(None),
        aux: RwSignal::new(None),
        audio: RwSignal::new(None),
        image: RwSignal::new(None),
        image_derived: RwSignal::new(None),
        candidate: RwSignal::new(None),
    };
    provide_context(editors);
    let wizard_open = RwSignal::new(false);
    let catalog_open = CatalogOpen(RwSignal::new(false));
    provide_context(catalog_open);
    let recipes_open = RecipesOpen(RwSignal::new(false));
    provide_context(recipes_open);

    let data = ModelsData::new();
    let catalog = use_model_catalog();
    let page = PageCtx {
        toasts: use_toasts(),
        reload,
        editors,
        data,
        ops: use_ops(),
        logs: RwSignal::new(None),
        refused: RwSignal::new(None),
        deleting: RwSignal::new(None),
        nav: Nav::new(),
        wizard_open,
    };

    // Every op bumps `reload`: the list is refetched into place, and the
    // shared catalog follows so no picker offers a model that just went away.
    Effect::new(move |prev: Option<u32>| {
        let n = reload.0.get();
        if prev.is_some() {
            data.load();
            catalog.refresh();
        }
        n
    });

    let params = leptos_router::hooks::use_params_map();
    let sel =
        Memo::new(move |_| params.with(|p| route::Sel::parse(p.get_str("class"), p.get_str("id"))));

    view! {
        <split::ModelsSplit page=page sel=sel/>
        <Modal open=wizard_open title="Add model from Hugging Face" fill=true>
            <super::wizard::HfWizard/>
        </Modal>
        <Modal open=catalog_open.0 title="Add audio model from the catalog" fill=true>
            <super::audio_catalog::AudioCatalogBrowser open=catalog_open.0/>
        </Modal>
        <Modal open=recipes_open.0 title="Add image model from a recipe" fill=true>
            <super::image_recipes::ImageRecipeBrowser open=recipes_open.0/>
        </Modal>
        // New rows of these classes are made in their modal editors, as
        // before; a saved row is edited in the detail pane.
        <super::model_editors::AliasEditor editing=editors.alias on_saved=move || bump(reload)/>
        <super::model_editors::AuxEditor editing=editors.aux on_saved=move || bump(reload)/>
        <super::model_editors::AudioEditor editing=editors.audio on_saved=move || bump(reload)/>
        <super::model_editors::ImageEditor editing=editors.image on_saved=move || bump(reload)/>
        <super::candidate_alias_editor::CandidateAliasEditor
            editing=editors.candidate
            full=page.data.full
            on_saved=move || bump(reload)
        />
        <LogsModal target=page.logs/>
        <DeleteModal page=page/>
        {move || {
            page.refused
                .get()
                .map(|(class, model_id, msg)| {
                    let mid = StoredValue::new(model_id);
                    view! {
                        <StopRefusedModal
                            message=msg
                            on_cancel=move || page.refused.set(None)
                            on_force=move || {
                                page.refused.set(None);
                                container_action(
                                    page.ops,
                                    page.toasts,
                                    class,
                                    mid.get_value(),
                                    "stop",
                                    true,
                                    |_| {},
                                );
                            }
                        />
                    }
                })
        }}
    }
}

/// Deleting a model asks in a modal that says what goes and what stays
/// (UX plan §4: a model, an upstream, an agent or a corpus keeps its
/// explaining modal), not in the row menu, where a double-click would answer
/// its own question.
#[component]
fn DeleteModal(page: PageCtx) -> impl IntoView {
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = page.deleting.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && page.deleting.with_untracked(Option::is_some) {
            page.deleting.set(None);
        }
    });
    view! {
        <Modal open=open title="Delete model">
            {move || {
                page.deleting
                    .get()
                    .map(|(class, id, name)| {
                        let what = match class {
                            Class::Alias => "The upstream model it points at is not touched.",
                            Class::Candidate => {
                                "The local models it points at are not touched, and keep running \
                                 if they were."
                            }
                            _ => "Its container is stopped; the files on disk stay.",
                        };
                        view! {
                            <p>
                                "Remove " <span class="mono-sm">{name}</span>
                                " from the gateway config? " {what}
                            </p>
                            <ModalFooter>
                                <button class="btn ghost" on:click=move |_| open.set(false)>
                                    "Keep it"
                                </button>
                                <button
                                    class="btn danger"
                                    on:click=move |_| {
                                        open.set(false);
                                        page.op(class.op(), json!({ "action": "delete", "id": id }));
                                    }
                                >
                                    "Delete"
                                </button>
                            </ModalFooter>
                        }
                    })
            }}
        </Modal>
    }
}
