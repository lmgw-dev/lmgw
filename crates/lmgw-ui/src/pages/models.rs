//! Models → Configured: everything this gateway is set up to serve — the four
//! local runtimes and the cloud aliases — as one grouped table, plus one line
//! per passthrough upstream pointing at its catalog (`model_catalog.rs`),
//! which is where the hundreds of models an upstream like kilo offers live.
//! HF-first: the primary action is the Hugging Face wizard; picking a GGUF
//! already on disk is the exception path (wired in the local-model editor).

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{
    AliasView, AudioModel, AuxModel, CandidateAliasView, ImageModel, ModelsFull, RuntimeStatus,
};
use serde_json::{json, Value};

use crate::catalog::use_model_catalog;
use crate::fmt::{age, grouped, hue_for, human_bytes, of};
use crate::model_ops::{container_action, LogsModal, StopRefusedModal};
use crate::ops_state::{model_key, use_ops, OpsState};
use crate::widgets::{
    filter_words, use_toasts, CopyBtn, Facet, FacetSet, FilterBar, GroupRow, MenuItem, Modal,
    ModalFooter, NavTab, PageFrame, PageMode, RowMenu, SubNav, Toasts, Tone,
};

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
        hold_fallback_mode: "inherit".into(),
        hold_fallback: None,
    }
}

/// A blank image model (id 0) opens `ImageEditor` in "create" mode. The
/// defaults are the row defaults `ops::image_model_set` applies when the patch
/// leaves them out (`idle_seconds` 300, `enabled`, `hold_fallback_mode`
/// inherit), so an untouched form saves as the same row the ops plane would
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
        hold_fallback_mode: "inherit".into(),
        hold_fallback: None,
        capabilities_override: None,
        // Learned by the gateway, never by the form: a row that has never run
        // has nothing to say about what a generation costs.
        peak_extra_bytes: None,
        peak_learned_at: None,
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
        args: Vec::new(),
        idle_seconds: 300,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: "inherit".into(),
        hold_fallback: None,
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

/// The two views of Models with their sizes: what is configured, and what
/// the passthrough upstreams offer. Hidden catalog models count — hidden is
/// "not advertised", not gone — and a catalog that failed to load says so.
#[component]
pub(super) fn ModelsTabs(
    #[prop(into)] configured: Signal<Option<usize>>,
    /// (models, hidden, catalogs that failed to load)
    #[prop(into)]
    catalogs: Signal<Option<(usize, usize, usize)>>,
) -> impl IntoView {
    let tabs = Signal::derive(move || {
        let mut conf = NavTab::new("Configured", "/models").exact();
        if let Some(n) = configured.get() {
            conf = conf.count(grouped(n as u64));
        }
        let mut cat = NavTab::new("Upstream catalogs", "/models/catalog");
        if let Some((n, hidden, failed)) = catalogs.get() {
            let mut c = format!("{} · {} hidden", grouped(n as u64), grouped(hidden as u64));
            if failed > 0 {
                c.push_str(&format!(" · {failed} failed"));
                cat = cat.tone(Tone::Bad);
            }
            cat = cat.count(c);
        }
        vec![conf, cat]
    });
    view! { <SubNav tabs=tabs/> }
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

/// Index of the passthrough group in the open-state array, after the classes.
const CATALOGS: usize = 6;
const COLS: u32 = 6;

impl Class {
    const ALL: [Self; 6] = [
        Self::Chat,
        Self::Aux,
        Self::Audio,
        Self::Image,
        Self::Alias,
        Self::Candidate,
    ];

    fn idx(self) -> usize {
        self as usize
    }

    /// The facet id, and the name its group's open state persists under.
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

    fn facet(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Aux => "Embed",
            Self::Audio => "Audio",
            Self::Image => "Image",
            Self::Alias => "Aliases",
            Self::Candidate => "Candidate aliases",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Chat => "Local · chat",
            Self::Aux => "Local · embeddings + rerank",
            Self::Audio => "Local · audio",
            Self::Image => "Local · image",
            Self::Alias => "Cloud aliases",
            Self::Candidate => "Candidate aliases",
        }
    }

    /// What `live.runtime` and the container op call the class. An alias
    /// has no container — a candidate alias either, even though the
    /// candidates it points at do (the row list shows each candidate's own
    /// state instead, in `CandidateStateCell`).
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
    fn probe(self) -> Option<&'static str> {
        match self {
            Self::Chat => Some("generates one token"),
            Self::Aux => Some("embeds or reranks a probe input"),
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
    /// A candidate alias's `gpu_deferred` count over the last 24 h (§6);
    /// always `None` for every other class.
    deferrals_24h: Option<u64>,
}

impl Row {
    fn matches(&self, words: &[String]) -> bool {
        if words.is_empty() {
            return true;
        }
        let hay = format!(
            "{} {} {} {} {}",
            self.name, self.model_id, self.kind_title, self.note, self.file_title
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
            deferrals_24h: None,
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
            kind_title: match &a.pooling {
                Some(p) => format!("{} · pooling {p}", a.kind),
                None => format!("{} · pooling from the GGUF", a.kind),
            },
            note: String::new(),
            file: file_name(&a.gguf_path),
            file_title: a.gguf_path.clone(),
            copy: a.gguf_path.clone(),
            ctx: a.ctx_size,
            enabled: a.enabled,
            problems: 0,
            deferrals_24h: None,
        });
    }
    for v in &m.audio {
        let a = &v.model;
        out.push(Row {
            class: Class::Audio,
            id: a.id,
            model_id: a.model_id.clone(),
            name: v.public_name.clone(),
            kind: a.task.clone(),
            kind_title: format!("{} · family {} · {} mode", a.task, a.family, a.mode),
            note: String::new(),
            file: short_dir(&a.path),
            file_title: a.path.clone(),
            copy: a.path.clone(),
            ctx: None,
            enabled: a.enabled,
            problems: 0,
            deferrals_24h: None,
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
            file.push_str(&format!(" · peak +{}", human_bytes(p)));
            title.push(format!(
                "peak +{} while generating: admission keeps this much free on top of the \
                 idle residency; a files or args change resets it",
                human_bytes(p)
            ));
        }
        out.push(Row {
            class: Class::Image,
            id: i.id,
            model_id: i.model_id.clone(),
            name: v.public_name.clone(),
            kind: if i.modes.is_empty() {
                "img_gen".into()
            } else {
                i.modes.join(" · ")
            },
            kind_title: "stable-diffusion.cpp pipeline".into(),
            note: if i.edit {
                "edits".into()
            } else {
                String::new()
            },
            file,
            file_title: title.join("\n"),
            copy: primary,
            ctx: None,
            enabled: i.enabled,
            problems: 0,
            deferrals_24h: None,
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

#[derive(Clone, PartialEq, Eq, Hash)]
enum Item {
    Group(Class),
    Row(Row),
    CatalogGroup,
    Catalog(CatalogLine),
}

/// What the table shows under the current filter, with the counts every
/// header needs: (shown, total) per class, then for the catalogs.
#[derive(Clone, PartialEq)]
struct Shown {
    items: Vec<Item>,
    counts: [(usize, usize); 7],
    rows: usize,
    total: usize,
}

/// The router's navigate, shareable by `Copy` handlers.
type Navigate = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// What a row reaches: every op goes through here, so each one refetches
/// the list and refreshes the shared catalog the same way. Captured at
/// render — event handlers run with no reactive owner to look context up in.
#[derive(Clone, Copy)]
struct PageCtx {
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
    navigate: StoredValue<Navigate>,
}

impl PageCtx {
    /// Fire a mutation op, toast the outcome, refetch. Container hygiene
    /// (stopping a container whose config just went stale) is the backend's
    /// job — `local_model_set`/`aux_model_set`/`audio_model_set` already do it
    /// on every update/enable/disable.
    fn op(self, name: &'static str, args: Value) {
        spawn_local(async move {
            match crate::api::post::<Value, _>(format!("/api/op/{name}"), &args).await {
                Ok(v) => {
                    let msg = v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("done")
                        .to_string();
                    self.toasts.ok(msg);
                    bump(self.reload);
                }
                Err(e) => self.toasts.err(e.to_string()),
            }
        });
    }

    fn edit(self, class: Class, id: i64) {
        let full = self.data.full;
        let find = |f: &ModelsFull| -> bool {
            match class {
                Class::Aux => f.aux.iter().find(|v| v.model.id == id).map(|v| {
                    self.editors.aux.set(Some(v.model.clone()));
                }),
                Class::Audio => f.audio.iter().find(|v| v.model.id == id).map(|v| {
                    self.editors.audio.set(Some(v.model.clone()));
                }),
                Class::Image => f.image.iter().find(|v| v.model.id == id).map(|v| {
                    self.editors.image.set(Some(v.model.clone()));
                }),
                Class::Alias => f.aliases.iter().find(|a| a.id == id).map(|a| {
                    self.editors.alias.set(Some(a.clone()));
                }),
                Class::Candidate => f.candidate_aliases.iter().find(|c| c.id == id).map(|c| {
                    self.editors.candidate.set(Some(c.clone()));
                }),
                Class::Chat => None,
            }
            .is_some()
        };
        if class == Class::Chat {
            self.navigate
                .with_value(|nav| nav(&format!("/models/local/{id}")));
        } else {
            full.with_untracked(|f| f.as_ref().map(find));
        }
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
fn run_test(toasts: Toasts, class: Class, model_id: String, kind: String, testing: RwSignal<bool>) {
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
        let res = crate::api::post::<Value, _>("/api/op/local_model_test", &body).await;
        testing.set(false);
        let v = match res {
            Ok(v) => v,
            Err(e) => return toasts.err(e.to_string()),
        };
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            let err = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("load test failed");
            let hint = v.get("hint").and_then(Value::as_str).unwrap_or_default();
            return toasts.err(if hint.is_empty() {
                err.to_string()
            } else {
                format!("{err} — {hint}")
            });
        }
        let ms = v
            .get("latency_ms")
            .and_then(Value::as_u64)
            .map(|ms| format!(" in {ms} ms"))
            .unwrap_or_default();
        toasts.ok(match class {
            Class::Aux => {
                let dims = v
                    .get("dimensions")
                    .and_then(Value::as_u64)
                    .map(|d| format!(" ({d} dimensions)"))
                    .unwrap_or_default();
                let verb = if kind == "rerank" {
                    "reranks"
                } else {
                    "embeds"
                };
                format!("{model_id}: loads and {verb}{dims}{ms}")
            }
            Class::Image => {
                let bytes = v
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .map(human_bytes)
                    .unwrap_or_else(|| "no image".into());
                let size = v.get("size").and_then(Value::as_str).unwrap_or_default();
                format!("{model_id}: drew {size}{ms} — {bytes}")
            }
            _ => v
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{model_id}: loads and generates{ms}")),
        });
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
    let live = crate::live::use_live();
    let navigate = leptos_router::hooks::use_navigate();
    let nav: Navigate = std::sync::Arc::new(move |href: &str| navigate(href, Default::default()));
    let page = PageCtx {
        toasts: use_toasts(),
        reload,
        editors,
        data,
        ops: use_ops(),
        logs: RwSignal::new(None),
        refused: RwSignal::new(None),
        deleting: RwSignal::new(None),
        navigate: StoredValue::new(nav),
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

    let rows = Memo::new(move |_| {
        data.full
            .with(|f| f.as_ref().map(rows_of).unwrap_or_default())
    });
    let lines = Memo::new(move |_| {
        data.full.with(|f| {
            f.as_ref()
                .map(|f| {
                    f.passthrough
                        .iter()
                        .map(|p| CatalogLine {
                            id: p.id,
                            name: p.upstream.clone(),
                            prefix: p.prefix.clone(),
                            hidden: p.hidden.len(),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    // Models per upstream as `/v1/models` lists them: hidden ones are left
    // out there, so each line adds its hidden count back.
    let listed = Memo::new(move |_| {
        catalog.entries.with(|es| {
            let mut m: HashMap<String, usize> = HashMap::new();
            for e in es.iter().filter(|e| !e.local) {
                *m.entry(e.source.clone()).or_default() += 1;
            }
            m
        })
    });
    let catalog_total =
        move |l: &CatalogLine| listed.with(|m| m.get(&l.name).copied().unwrap_or(0)) + l.hidden;

    let query = crate::url_state::use_query_signal("q");
    let facet = crate::url_state::use_query_signal("show");
    let open: [RwSignal<bool>; 7] = std::array::from_fn(|i| {
        let id = Class::ALL.get(i).map(|c| c.id()).unwrap_or("catalogs");
        crate::prefs::persisted_bool(&format!("open.models.group.{id}"), true)
    });

    let shown = Memo::new(move |_| {
        let mut s = Shown {
            items: Vec::new(),
            counts: [(0, 0); 7],
            rows: 0,
            total: 0,
        };
        // Nothing is known yet (or the first load failed): no group may say
        // "none yet — add one" about a list that never arrived (review
        // code:M1). The Loading row or the error says where things are.
        if data.full.with(Option::is_none) {
            return s;
        }
        let words = filter_words(&query.get());
        let f = facet.get();
        let narrowed = !words.is_empty() || !f.is_empty();
        // Only the Running facet reads the runtime frame, so the table is
        // not re-derived on every frame the bus sends.
        let rt = if f == "running" {
            live.runtime.get().unwrap_or_default()
        } else {
            Vec::new()
        };
        let keep = |r: &Row| {
            r.matches(&words)
                && match f.as_str() {
                    "" => true,
                    "disabled" => !r.enabled,
                    "running" => is_running(&rt, r),
                    id => r.class.id() == id,
                }
        };
        rows.with(|rs| {
            s.total = rs.len();
            for c in Class::ALL {
                let all: Vec<&Row> = rs.iter().filter(|r| r.class == c).collect();
                let mine: Vec<&Row> = all.iter().copied().filter(|r| keep(r)).collect();
                s.counts[c.idx()] = (mine.len(), all.len());
                s.rows += mine.len();
                // A class's own facet keeps its group even with nothing in
                // it: the header is where the way to add the first one is.
                if narrowed && mine.is_empty() && f != c.id() {
                    continue;
                }
                s.items.push(Item::Group(c));
                // A filter holds every group open: a folded group would hide
                // the very matches its count reports.
                if narrowed || open[c.idx()].get() {
                    s.items.extend(mine.into_iter().cloned().map(Item::Row));
                }
            }
        });
        // The catalogs are not configured models: a class or state facet
        // leaves them out, a text filter matches the upstream's name.
        lines.with(|ls| {
            let hits: Vec<&CatalogLine> = ls
                .iter()
                .filter(|l| {
                    f.is_empty()
                        && words.iter().all(|w| {
                            format!("{} {}", l.name, l.prefix)
                                .to_lowercase()
                                .contains(w.as_str())
                        })
                })
                .collect();
            let models: usize = ls.iter().map(catalog_total).sum();
            s.counts[CATALOGS] = (models, models);
            if ls.is_empty() || (narrowed && hits.is_empty()) {
                return;
            }
            s.items.push(Item::CatalogGroup);
            if narrowed || open[CATALOGS].get() {
                s.items.extend(hits.into_iter().cloned().map(Item::Catalog));
            }
        });
        s
    });

    let facets = Signal::derive(move || {
        if data.full.with(Option::is_none) {
            return Vec::new();
        }
        let rt = live.runtime.get().unwrap_or_default();
        rows.with(|rs| {
            let mut v: Vec<Facet> = Class::ALL
                .iter()
                .map(|c| Facet {
                    id: c.id().into(),
                    label: c.facet().into(),
                    count: rs.iter().filter(|r| r.class == *c).count(),
                })
                .collect();
            v.push(Facet {
                id: "disabled".into(),
                label: "Disabled".into(),
                count: rs.iter().filter(|r| !r.enabled).count(),
            });
            v.push(Facet {
                id: "running".into(),
                label: "Running".into(),
                count: rs.iter().filter(|r| is_running(&rt, r)).count(),
            });
            v
        })
    });

    let configured =
        Signal::derive(move || data.full.with(|f| f.as_ref().map(|_| rows.with(Vec::len))));
    let catalogs = Signal::derive(move || {
        data.full.with(|f| {
            f.as_ref().map(|_| {
                lines.with(|ls| {
                    let hidden = ls.iter().map(|l| l.hidden).sum::<usize>();
                    (ls.iter().map(catalog_total).sum::<usize>(), hidden, 0)
                })
            })
        })
    });

    let group_count = move |i: usize| {
        Signal::derive(move || {
            shown.with(|s| {
                let (n, t) = s.counts[i];
                of(n, t)
            })
        })
    };

    let render = move |it: Item| -> AnyView {
        match it {
            Item::Group(c) => view! {
                <GroupRow
                    colspan=COLS
                    label=c.title()
                    count=group_count(c.idx())
                    open=open[c.idx()]
                    meta=move || move || group_meta(c, shown, rows, live)
                    actions=move || group_actions(c, editors, catalog_open, recipes_open)
                />
            }
            .into_any(),
            Item::Row(r) => view! { <ModelRow r=r page=page/> }.into_any(),
            Item::CatalogGroup => view! {
                <GroupRow
                    colspan=COLS
                    label="Upstream catalogs"
                    count=group_count(CATALOGS)
                    open=open[CATALOGS]
                    meta=|| "every model of an expose-all upstream, resolved live"
                    actions=|| {
                        view! {
                            <a class="btn ghost sm" href="/upstreams">
                                "Upstreams"
                            </a>
                        }
                    }
                />
            }
            .into_any(),
            Item::Catalog(l) => {
                let total = Signal::derive({
                    let l = l.clone();
                    move || catalog_total(&l)
                });
                view! { <CatalogRow l=l total=total loading=catalog.loading/> }.into_any()
            }
        }
    };

    view! {
        <PageFrame
            title="Models"
            sub="everything the gateway can serve"
            mode=PageMode::Fill
            head_extra=move || view! { <ModelsTabs configured=configured catalogs=catalogs/> }
            actions=move || {
                view! {
                    // Also in its group's header, which a long local list
                    // pushes below the fold: an alias is half of what this
                    // page configures, so its way in stays on screen.
                    <button
                        class="btn"
                        title="Map a name clients request to a model of an upstream"
                        on:click=move |_| editors.alias.set(Some(AliasView::default()))
                    >
                        "New alias"
                    </button>
                    <button class="btn primary" on:click=move |_| wizard_open.set(true)>
                        "Add from Hugging Face"
                    </button>
                }
            }
            toolbar=move || {
                view! {
                    <FilterBar
                        query=query
                        placeholder="Filter by name or file"
                        shown=Signal::derive(move || shown.with(|s| s.rows))
                        total=Signal::derive(move || shown.with(|s| s.total))
                        noun="models"
                        facets=FacetSet { items: facets, active: facet }
                        status=Signal::derive(move || {
                            match (data.full.with(Option::is_some), data.error.with(Option::is_some)) {
                                (true, _) => String::new(),
                                (false, false) => "Loading…".to_string(),
                                (false, true) => "not loaded".to_string(),
                            }
                        })
                    />
                }
            }
        >
            {move || {
                data.error
                    .get()
                    .map(|e| {
                        let stale = data.full.with(Option::is_some);
                        view! {
                            <div class="notice err row">
                                {if stale { "Refreshing the list failed: " } else { "Loading models failed: " }}
                                {e}
                                <button class="btn ghost sm" on:click=move |_| data.load()>
                                    "Retry"
                                </button>
                            </div>
                        }
                    })
            }}
            <div class="fill-pane card pad0">
                <table class="data">
                    <thead>
                        <tr>
                            <th>"Model"</th>
                            <th class="col-p2">"Type"</th>
                            <th>"File"</th>
                            <th class="col-p3 num-h">"Ctx"</th>
                            <th title="Where its container is; a disabled model is not served at all">
                                "State"
                            </th>
                            <th></th>
                        </tr>
                    </thead>
                    <tbody>
                        <Show when=move || data.full.with(Option::is_none) && data.error.with(Option::is_none)>
                            <tr>
                                <td colspan=COLS class="dim">"Loading…"</td>
                            </tr>
                        </Show>
                        <For each=move || shown.with(|s| s.items.clone()) key=|it| it.clone() let:it>
                            {render(it)}
                        </For>
                    </tbody>
                </table>
            </div>

            <Modal open=wizard_open title="Add model from Hugging Face" fill=true>
                <super::wizard::HfWizard/>
            </Modal>
            <Modal open=catalog_open.0 title="Add audio model from the catalog" fill=true>
                <super::audio_catalog::AudioCatalogBrowser open=catalog_open.0/>
            </Modal>
            <Modal open=recipes_open.0 title="Add image model from a recipe" fill=true>
                <super::image_recipes::ImageRecipeBrowser open=recipes_open.0/>
            </Modal>
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
        </PageFrame>
    }
}

/// A group header's dim note: what a class with nothing in it is waiting
/// for, or how many of its models are up.
fn group_meta(
    c: Class,
    shown: Memo<Shown>,
    rows: Memo<Vec<Row>>,
    live: crate::live::LiveBus,
) -> String {
    if shown.with(|s| s.counts[c.idx()].1) == 0 {
        return c.empty().to_string();
    }
    let rt = live.runtime.get().unwrap_or_default();
    let (up, off) = rows.with(|rs| {
        let mine = rs.iter().filter(|r| r.class == c);
        (
            mine.clone().filter(|r| is_running(&rt, r)).count(),
            mine.filter(|r| !r.enabled).count(),
        )
    });
    let mut parts = Vec::new();
    if up > 0 {
        parts.push(format!("{up} running"));
    }
    if off > 0 {
        parts.push(format!("{off} disabled"));
    }
    parts.join(" · ")
}

/// Each group carries the ways to add to it.
fn group_actions(
    c: Class,
    editors: Editors,
    catalog_open: CatalogOpen,
    recipes_open: RecipesOpen,
) -> AnyView {
    match c {
        Class::Chat => view! {
            <a class="btn ghost sm" href="/models/local/new" title="Wire up a GGUF that is already in the models dir">
                "From disk"
            </a>
        }
        .into_any(),
        Class::Aux => view! {
            <button
                class="btn ghost sm"
                title="Wire up an embedding or rerank GGUF that is already in the aux models dir"
                on:click=move |_| editors.aux.set(Some(blank_aux_model()))
            >
                "New"
            </button>
        }
        .into_any(),
        Class::Audio => view! {
            <button class="btn ghost sm" on:click=move |_| editors.audio.set(Some(blank_audio_model()))>
                "New"
            </button>
            <button
                class="btn ghost sm"
                title="Browse audio.cpp's model families and install a package"
                on:click=move |_| catalog_open.0.set(true)
            >
                "Add from catalog"
            </button>
        }
        .into_any(),
        Class::Image => view! {
            <button class="btn ghost sm" on:click=move |_| editors.image.set(Some(blank_image_model()))>
                "New"
            </button>
            <button
                class="btn ghost sm"
                title="Shipped stable-diffusion.cpp pipelines — download every component, then fill the editor"
                on:click=move |_| recipes_open.0.set(true)
            >
                "Add from recipe"
            </button>
        }
        .into_any(),
        Class::Alias => view! {
            <button class="btn ghost sm" on:click=move |_| editors.alias.set(Some(AliasView::default()))>
                "New alias"
            </button>
        }
        .into_any(),
        Class::Candidate => view! {
            <button
                class="btn ghost sm"
                title="A client-facing name backed by a primary local model plus alternates, used only when already loaded"
                on:click=move |_| editors
                    .candidate
                    .set(Some(super::candidate_alias_editor::blank_candidate_alias()))
            >
                "Add candidate alias"
            </button>
        }
        .into_any(),
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

/// Where a model's container is, from the runtime frame: idle is the normal
/// state of an on-demand model, so it is neutral, not a warning, and green is
/// kept for what is actually up. (A disabled row says so instead, and is
/// dimmed: being switched off is its whole state.)
#[component]
fn RuntimeCell(class: Class, model_id: String) -> impl IntoView {
    let live = crate::live::use_live();
    let mid = StoredValue::new(model_id);
    let rt = move || {
        live.runtime.with(|rows| {
            rows.as_deref()
                .and_then(|rows| mid.with_value(|m| runtime_of(rows, class, m).cloned()))
        })
    };
    move || {
        let r = rt();
        let (cls, label) = match r.as_ref().map(|r| r.state.as_str()) {
            Some("ready") => (
                "chip ok",
                format!(
                    "running · {}",
                    age(r.as_ref().map_or(0, |r| r.started_at_age_seconds))
                ),
            ),
            Some("starting") => ("chip live", "starting".to_string()),
            Some("stopping") => ("chip live", "stopping".to_string()),
            Some(other) => ("chip live", other.to_string()),
            None => ("chip off", "idle".to_string()),
        };
        // An image start that found problems keeps them here, on the row,
        // rather than only in the Overview table.
        let warnings = r.as_ref().map(|r| r.warnings.clone()).unwrap_or_default();
        // Ownership (candidate-aliases design §4.4, §6): absent is the
        // owner's, and there is nothing to say about the normal case — a
        // badge only ever appears for the exception.
        let background = r
            .as_ref()
            .is_some_and(|r| r.owner.as_deref() == Some("background"));
        let draining = r.as_ref().is_some_and(|r| r.draining_for_owner);
        view! {
            <span class=cls>
                <span class="dot"></span>
                {label}
            </span>
            {background
                .then(|| {
                    view! {
                        <span
                            class="type-badge"
                            style="margin-left:4px"
                            title="Started by a background candidate alias request; the owner has not claimed it since"
                        >
                            "background"
                        </span>
                    }
                })}
            {draining
                .then(|| {
                    view! {
                        <span
                            class="chip warn"
                            style="margin-left:4px"
                            title="An owner request is waiting for room on this GPU; background traffic skips this model until it is idle"
                        >
                            "draining for owner"
                        </span>
                    }
                })}
            {(!warnings.is_empty())
                .then(|| {
                    view! {
                        <span class="chip warn" style="margin-left:4px" title=warnings.join("\n")>
                            {format!("{} warning{}", warnings.len(), if warnings.len() == 1 { "" } else { "s" })}
                        </span>
                    }
                })}
        }
    }
}

#[component]
fn ModelRow(r: Row, page: PageCtx) -> impl IntoView {
    let class = r.class;
    let id = r.id;
    let enabled = r.enabled;
    let mid = StoredValue::new(r.model_id.clone());
    let kind = StoredValue::new(r.kind.clone());
    let name_for_delete = StoredValue::new(r.name.clone());
    let hue = hue_for(&r.name);
    let testing = RwSignal::new(false);
    let live = crate::live::use_live();
    let navigate = StoredValue::new(leptos_router::hooks::use_navigate());

    let chip = view! {
        <span class="model-chip" style=format!("--hue:{hue}")>
            <i></i>
            {r.name.clone()}
        </span>
    };
    // The name opens the editor: a routed page for chat, the class's modal
    // for the rest.
    let name = if class == Class::Chat {
        view! {
            <a href=format!("/models/local/{id}") class="row-link" title="Open the editor">
                {chip}
            </a>
        }
        .into_any()
    } else {
        view! {
            <button type="button" class="link-btn row-link" title="Open the editor" on:click=move |_| page.edit(class, id)>
                {chip}
            </button>
        }
        .into_any()
    };

    let items = Signal::derive(move || {
        let mut v = vec![MenuItem::new("Edit", move || page.edit(class, id))];
        if class == Class::Chat {
            v.push(
                MenuItem::new("Duplicate", move || {
                    page.op(
                        "local_model_set",
                        json!({ "action": "duplicate", "id": id }),
                    )
                })
                .title("Clone under a fresh id — same GGUF, params, args and flags"),
            );
            // Benchmark design §8.2: the New benchmark modal, this row picked.
            v.push(
                MenuItem::new("Benchmark…", move || {
                    let href = super::benchmarks::new_href(Some(&mid.get_value()), None);
                    navigate.with_value(|n| n(&href, Default::default()));
                })
                .title("Measure this row: load, probes, prefill, decode, concurrent and mixed load, tokens per joule — it takes the whole GPU while it runs"),
            );
        }
        let toggle = if enabled { "disable" } else { "enable" };
        v.push(MenuItem::new(
            if enabled { "Disable" } else { "Enable" },
            move || page.op(class.op(), json!({ "action": toggle, "id": id })),
        ));
        if let Some(rc) = class.runtime() {
            let busy = mid.with_value(|m| page.ops.busy(&model_key(rc, m)));
            let up = live.runtime.with(|rows| {
                rows.as_deref()
                    .is_some_and(|rows| mid.with_value(|m| runtime_of(rows, class, m).is_some()))
            });
            // A disabled model is not served: a container started for it
            // would hold VRAM for nothing (the start op does not check), so
            // Start and Restart wait for Enable. Stop stays — that is how
            // one left running is let go (review code:M4).
            const OFF: &str = "Enable it first: a disabled model is not served";
            if up {
                v.push(
                    MenuItem::new("Stop", move || page.container(rc, mid.get_value(), "stop"))
                        .disabled(busy),
                );
                v.push(
                    MenuItem::new("Restart", move || {
                        page.container(rc, mid.get_value(), "restart")
                    })
                    .disabled(busy || !enabled)
                    .title(if enabled {
                        "Stop and start its container again, with the saved configuration"
                    } else {
                        OFF
                    }),
                );
            } else {
                v.push(
                    MenuItem::new("Start", move || {
                        page.container(rc, mid.get_value(), "start")
                    })
                    .disabled(busy || !enabled)
                    .title(if enabled {
                        "Start its container now; may evict other models under the VRAM ledger"
                    } else {
                        OFF
                    }),
                );
            }
            v.push(
                MenuItem::new("Logs", move || page.logs.set(Some((rc, mid.get_value()))))
                    .title("podman logs for this model's container"),
            );
        }
        v.push(
            MenuItem::new("Delete…", move || {
                page.deleting
                    .set(Some((class, id, name_for_delete.get_value())))
            })
            .title(match class {
                Class::Alias | Class::Candidate => "Remove the alias from the gateway config",
                _ => {
                    "Remove it from the gateway config and stop its container; the files on \
                      disk stay"
                }
            }),
        );
        v
    });

    let test = class.probe().map(|probe| {
        view! {
            <button
                class="btn sm"
                disabled=move || testing.get() || !enabled
                title=if enabled {
                    format!(
                        "Load test: starts this model's container if it is not running (which may evict others under the VRAM ledger), then {probe}"
                    )
                } else {
                    "Enable it first: a disabled model is not served, so there is nothing to test".to_string()
                }
                on:click=move |_| run_test(page.toasts, class, mid.get_value(), kind.get_value(), testing)
            >
                {move || if testing.get() { "Testing…" } else { "Test" }}
            </button>
        }
    });

    view! {
        <tr class:muted=!enabled>
            <td>{name}</td>
            <td class="col-p2">
                <span class="type-badge" title=r.kind_title.clone()>
                    {r.kind.clone()}
                </span>
                {(!r.note.is_empty()).then(|| view! { <span class="dim cell-note">{r.note.clone()}</span> })}
            </td>
            <td class="clip mono-sm dim" title=r.file_title.clone()>
                {(!r.copy.is_empty()).then(|| view! { <CopyBtn text=r.copy.clone() title="Copy the full path"/> })}
                {r.file.clone()}
            </td>
            <td class="num col-p3">{r.ctx.map(|c| grouped(c as u64)).unwrap_or_else(|| "—".into())}</td>
            <td>
                {if !enabled {
                    view! {
                        <span class="chip off" title="Not served; ⋯ → Enable brings it back">
                            <span class="dot"></span>
                            "disabled"
                        </span>
                    }
                        .into_any()
                } else if class.runtime().is_some() {
                    view! { <RuntimeCell class=class model_id=r.model_id.clone()/> }.into_any()
                } else if class == Class::Candidate {
                    view! {
                        <super::candidate_alias_editor::CandidateStateCell
                            problems=r.problems
                            deferrals_24h=r.deferrals_24h
                        />
                    }
                        .into_any()
                } else {
                    view! { <span class="dim" title="Served by its upstream on each request; it has no container">"—"</span> }
                        .into_any()
                }}
            </td>
            <td class="actions">{test} <RowMenu items=items/></td>
        </tr>
    }
}

/// One passthrough upstream: its request prefix, its size, and the way to
/// its catalog.
#[component]
fn CatalogRow(l: CatalogLine, total: Signal<usize>, loading: RwSignal<bool>) -> impl IntoView {
    let hue = hue_for(&l.name);
    let href = format!(
        "/models/catalog?upstream={}",
        String::from(js_sys::encode_uri_component(&l.name))
    );
    let prefix = StoredValue::new(l.prefix.trim_matches('/').to_string());
    let hidden = l.hidden;
    let line = move || {
        let n = total.get();
        let size = if n == 0 && loading.get() {
            "counting…".to_string()
        } else if n == 0 {
            "no models listed right now — the catalog view says why".to_string()
        } else {
            format!(
                "{} models · {} hidden",
                grouped(n as u64),
                grouped(hidden as u64)
            )
        };
        format!("requests as {}/<id> · {size}", prefix.get_value())
    };
    view! {
        <tr>
            <td>
                <span class="model-chip" style=format!("--hue:{hue}")>
                    <i></i>
                    {l.name.clone()}
                </span>
            </td>
            <td class="col-p2">
                <span class="type-badge">"catalog"</span>
            </td>
            <td class="clip dim" title=line>{line}</td>
            <td class="num col-p3">"—"</td>
            <td>
                <span class="dim" title="Served by the upstream on each request">"—"</span>
            </td>
            <td class="actions">
                <a class="btn sm" href=href>"Open catalog"</a>
            </td>
        </tr>
    }
}
