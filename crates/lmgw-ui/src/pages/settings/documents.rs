//! Settings › Documents (document-conversion design §8): what files lmgw
//! can turn into text on this computer, the optional tools it found with
//! their versions, and the missing ones with what installing each would
//! give and how. Read from `doc_tools` each time the category is shown;
//! "Check again" is `doc_tools_check`. Nothing here is part of the draft.
//!
//! Plain words for everyone; the page's own expert switch (the first page
//! with one, the newcomer direction's per-page switch) adds the handler ids,
//! binary paths, licences, language packs, audio decoders and a failed
//! probe's output to the same rows, never a second view, and its own row:
//! "Parallel tool runs", the tool pool's size with what it was derived from,
//! set by hand with `settings_set`'s `documents_tool_slots` (design §7.7).
//! "Who calls the docling route" ([`callers`]) sums what the tools that call
//! the docling-serve routes send (`doc_adapter_usage`, design §7.8). "Web
//! addresses" ([`web`]) is whether the docling adapter downloads web
//! addresses, and from which sites (ruling 9). "Layout model" ([`models`])
//! is the model file lmgw can lay PDF pages out with: found or not, its
//! download and its switch (WP8). "Read pages with a model" ([`model`]) is
//! the page-reading model, off until one is chosen (design §8, D5);
//! "Describe pictures with" ([`pictures`]) the picture-description model
//! (WP11) and "Transcribe audio with" ([`speech`]) the model audio and
//! videos are transcribed with (WP7), in the same group and each off until
//! one is chosen too.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::documents::{
    DocCaps, ExtraKind, FormatRow, HintKind, InstallHint, MissingRow, Quality, ToolExtra, ToolRow,
    ToolState,
};
use serde_json::json;

use super::{f, Ctl, Def};

/// "How much conversions may hold": answers in flight, job storage, jobs
/// per key.
mod bounds;
/// "Who calls the docling route".
mod callers;
/// "Programs run confined", above the found tools.
mod confined;
/// "Read pages with a model", the page-reading model.
mod model;
/// "Layout model": the model files, their download and their switch.
mod models;
/// "Describe pictures with", the picture-description model.
mod pictures;
/// "Parallel tool runs", the expert view's tool-pool row.
mod pool;
/// "Transcribe audio with", the model audio and videos are transcribed with.
mod speech;
/// Retention's "Stored conversions".
mod stored;
/// "Web addresses": the docling adapter's web sources.
mod web;
use crate::widgets::{use_toasts, Switch};
use pool::PoolRow;

/// The three groups' rows. Each is one panel of its group's items.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum DocPart {
    Convert,
    Found,
    Missing,
    /// The tool pool's size (expert view only).
    Pool,
    /// Whether docling callers may send web addresses, and the sites.
    Web,
    /// The model files lmgw can use for converting (the layout model).
    Models,
    /// The page-reading model.
    Model,
    /// The picture-description model.
    Pictures,
    /// The speech-to-text model.
    Speech,
    /// Who calls the docling-serve routes, and what they send.
    Callers,
    /// What the background conversions hold on disk (Settings › Retention).
    Stored,
    /// The memory answers in flight hold.
    Answers,
    /// "Storage for conversion jobs".
    JobStorage,
    /// "Jobs per key".
    JobsPerKey,
}

/// What the search finds the category by, beyond the labels.
const TERMS: &str = "documents files convert conversion text pdf poppler ocr scan scanned \
     tesseract pandoc ffmpeg libreoffice word excel powerpoint opendocument office install \
     package tools docling";

pub(super) const DOCUMENTS: &[Def] = &[
    f(
        "documents",
        "doc-convert",
        "documents.convert",
        "What you can convert",
        Ctl::Documents(DocPart::Convert),
    )
    .terms(TERMS),
    f(
        "documents",
        "doc-found",
        "documents.found",
        "Found on this computer",
        Ctl::Documents(DocPart::Found),
    )
    .terms(TERMS),
    f(
        "documents",
        "doc-missing",
        "documents.missing",
        "Not installed",
        Ctl::Documents(DocPart::Missing),
    )
    .terms(TERMS),
    f(
        "documents",
        "doc-found",
        "documents.tool_slots",
        "Parallel tool runs",
        Ctl::Documents(DocPart::Pool),
    )
    .terms("parallel tool runs pool slots cores memory concurrency"),
    f(
        "documents",
        "doc-web",
        "documents.fetch_sources",
        "Let callers send web addresses",
        Ctl::Documents(DocPart::Web),
    )
    .terms(
        "web address addresses url urls http https fetch download sites allowed hosts docling \
         sources",
    ),
    f(
        "documents",
        "doc-models",
        "documents.onnx_layout",
        "Layout model",
        Ctl::Documents(DocPart::Models),
    )
    .terms("layout model onnx docling heron paragraphs reading order download processor cpu"),
    f(
        "documents",
        "doc-model",
        "documents.page_reader",
        "Read pages with a model",
        Ctl::Documents(DocPart::Model),
    )
    .terms("model help vision read pages scanned scans tables formulas code doctags hold fallback"),
    f(
        "documents",
        "doc-model",
        "documents.picture_alias",
        "Describe pictures with",
        Ctl::Documents(DocPart::Pictures),
    )
    .terms("model help vision describe pictures images figures description docling hold fallback"),
    f(
        "documents",
        "doc-model",
        "documents.asr_alias",
        "Transcribe audio with",
        Ctl::Documents(DocPart::Speech),
    )
    .terms("speech to text stt asr transcribe transcription audio video sound whisper parakeet"),
    f(
        "documents",
        "doc-callers",
        "documents.callers",
        "Who calls the docling route",
        Ctl::Documents(DocPart::Callers),
    )
    .terms("docling docling-serve callers keys options ocr engine adapter who calls"),
    f(
        "documents",
        "doc-bounds",
        "documents.answers",
        "Memory for answers",
        Ctl::Documents(DocPart::Answers),
    )
    .terms("memory answers in flight bound lease slow clients write deadline"),
    f(
        "documents",
        "doc-bounds",
        "documents.job_storage_mib",
        "Storage for conversion jobs",
        Ctl::Documents(DocPart::JobStorage),
    )
    .terms("storage disk space background conversion jobs async quota results uploads"),
    f(
        "documents",
        "doc-bounds",
        "documents.jobs_per_key",
        "Jobs per key",
        Ctl::Documents(DocPart::JobsPerKey),
    )
    .terms("jobs per key background conversion async queue quota limit callers"),
    f(
        "retention",
        "conversions",
        "documents.stored",
        "Stored conversions",
        Ctl::Documents(DocPart::Stored),
    )
    .terms("documents conversion convert job jobs stored disk space queue waiting"),
];

/// The page's state, shared by its hero and its three panels.
#[derive(Clone, Copy)]
pub(super) struct Docs {
    caps: RwSignal<Option<DocCaps>>,
    err: RwSignal<Option<String>>,
    busy: RwSignal<bool>,
    /// The page's expert switch, kept in this window's preferences.
    expert: RwSignal<bool>,
}

/// Made once by the Settings page.
pub(super) fn provide() {
    provide_context(Docs {
        caps: RwSignal::new(None),
        err: RwSignal::new(None),
        busy: RwSignal::new(false),
        expert: crate::prefs::persisted_bool("settings.documents.expert", false),
    });
}

pub(super) fn use_docs() -> Docs {
    expect_context::<Docs>()
}

impl Docs {
    /// Read the answer: the last check (`doc_tools`), or a new one
    /// (`doc_tools_check`).
    pub(super) fn load(self, again: bool) {
        if self.busy.get_untracked() {
            return;
        }
        self.busy.set(true);
        let op = if again {
            "/api/op/doc_tools_check"
        } else {
            "/api/op/doc_tools"
        };
        spawn_local(async move {
            match crate::api::post::<DocCaps, _>(op, &json!({})).await {
                Ok(c) => {
                    self.caps.try_set(Some(c));
                    self.err.try_set(None);
                }
                Err(e) => {
                    self.err.try_set(Some(e.to_string()));
                }
            }
            self.busy.try_set(false);
        });
    }
}

/// When the tools were checked, "Check again" and the expert switch: in the
/// category's hero.
#[component]
pub(super) fn HeroActions() -> impl IntoView {
    let d = use_docs();
    let checked = move || {
        d.caps.with(|c| {
            c.as_ref()
                .map(|c| format!("Checked {}", crate::fmt::rel_time(c.checked_at as f64)))
        })
    };
    view! {
        <div class="doc-hero-actions">
            <span class="dim doc-checked">{checked}</span>
            <button
                type="button"
                class="btn sm"
                disabled=move || d.busy.get()
                on:click=move |_| d.load(true)
            >
                {move || if d.busy.get() { "Checking…" } else { "Check again" }}
            </button>
            <label class="doc-expert-switch">
                <Switch value=d.expert label="Expert view"/>
                <span>"Expert view"</span>
            </label>
        </div>
    }
}

/// One group's panel: its rows, or why there are none yet.
#[component]
pub(super) fn DocPanel(part: DocPart, id: String, hidden: Signal<bool>) -> AnyView {
    if part == DocPart::Stored {
        return view! { <stored::Stored id=id hidden=hidden/> }.into_any();
    }
    let d = use_docs();
    // Every hidden → shown change reads the answer again (the panels of one
    // category show together, and the first one's read is the only one).
    Effect::new(move |_| {
        if !hidden.get() {
            d.load(false);
        }
    });
    if part == DocPart::Callers {
        return view! { <callers::Callers id=id hidden=hidden expert=d.expert/> }.into_any();
    }
    let body = move || {
        if let Some(e) = d.err.get() {
            return view! {
                <div class="prow notice-row warn">
                    {format!("Reading what this computer can convert failed: {e}")}
                    <button class="btn sm" on:click=move |_| d.load(false)>"Retry"</button>
                </div>
            }
            .into_any();
        }
        let Some(caps) = d.caps.get() else {
            return view! { <div class="prow notice-row">"Checking…"</div> }.into_any();
        };
        let expert = d.expert;
        match part {
            DocPart::Convert => caps
                .formats
                .into_iter()
                .map(|r| format_row(r, expert))
                .collect_view()
                .into_any(),
            DocPart::Found => {
                let rows: Vec<ToolRow> = caps
                    .tools
                    .into_iter()
                    .filter(|t| t.state != ToolState::Missing)
                    .collect();
                if rows.is_empty() {
                    return view! {
                        <div class="prow notice-row">"None of the optional tools is installed."</div>
                    }
                    .into_any();
                }
                let confined = view! {
                    <confined::ConfinedRow confinement=caps.confinement tools=rows.clone()/>
                };
                let tools = rows.into_iter().map(|t| tool_row(t, expert)).collect_view();
                view! { {confined} {tools} }.into_any()
            }
            DocPart::Pool => {
                view! { <PoolRow pool=caps.pool callers=caps.answers.callers/> }.into_any()
            }
            DocPart::Web => view! { <web::WebRows web=caps.web/> }.into_any(),
            DocPart::Models => view! { <models::ModelRows rows=caps.models/> }.into_any(),
            DocPart::Model => view! { <model::ModelRow reader=caps.page_reader/> }.into_any(),
            DocPart::Pictures => {
                view! { <pictures::PictureRow describer=caps.picture_describer/> }.into_any()
            }
            DocPart::Speech => view! { <speech::SpeechRow model=caps.speech/> }.into_any(),
            DocPart::Answers => view! { <bounds::AnswersRow answers=caps.answers/> }.into_any(),
            DocPart::JobStorage => {
                view! { <bounds::StorageRow bounds=caps.job_bounds/> }.into_any()
            }
            DocPart::JobsPerKey => view! { <bounds::PerKeyRow bounds=caps.job_bounds/> }.into_any(),
            DocPart::Callers | DocPart::Stored => ().into_any(),
            DocPart::Missing => {
                if caps.missing.is_empty() {
                    return view! {
                        <div class="prow notice-row">"Every optional tool is installed."</div>
                    }
                    .into_any();
                }
                caps.missing
                    .into_iter()
                    .map(|m| missing_row(m, expert))
                    .collect_view()
                    .into_any()
            }
        }
    };
    // The pool's row is the expert view's alone.
    let expert = d.expert;
    let gone = move || hidden.get() || (part == DocPart::Pool && !expert.get());
    view! {
        <div class="doc-rows" id=id hidden=gone>
            {body}
        </div>
    }
    .into_any()
}

/// The expert view's line under a row.
fn expert_line(expert: RwSignal<bool>, text: String) -> impl IntoView {
    (!text.is_empty()).then(|| {
        view! { <div class="prow-extra doc-expert" hidden=move || !expert.get()>{text}</div> }
    })
}

fn dot_class(q: Quality) -> &'static str {
    match q {
        Quality::Full => "doc-dot full",
        Quality::Partial => "doc-dot partial",
        Quality::None => "doc-dot none",
    }
}

/// What a kind of file gets, with its handler for the expert.
fn format_row(r: FormatRow, expert: RwSignal<bool>) -> impl IntoView {
    let warn = r.quality != Quality::Full;
    let detail = format!(
        "{} · {}",
        r.handler
            .as_deref()
            .map_or_else(|| "no handler".to_string(), |h| format!("handler {h}")),
        r.formats.join(", ")
    );
    view! {
        <div class="prow doc-row" data-format=r.id>
            <span class=dot_class(r.quality)></span>
            <div class="prow-text">
                <span class="prow-title">{r.label}</span>
                <span class="prow-sub" class:doc-warn=warn>{r.said}</span>
            </div>
            {expert_line(expert, detail)}
        </div>
    }
}

/// An extra in plain words, for the expert line.
fn extra_said(e: &ToolExtra) -> String {
    let what = match e.kind {
        ExtraKind::Binaries => "binaries",
        ExtraKind::Languages => "language packs",
        ExtraKind::AudioDecoders => "audio decoders",
        ExtraKind::Components => "parts",
    };
    let mut s = format!(
        "{what}: {}",
        if e.present.is_empty() {
            "none".to_string()
        } else {
            e.present.join(", ")
        }
    );
    if !e.absent.is_empty() {
        s.push_str(&format!(" (not installed: {})", e.absent.join(", ")));
    }
    s
}

/// What a tool is used for here, in plain words.
pub(super) fn used_for(t: &ToolRow) -> String {
    if t.used_for.is_empty() {
        "Not used by lmgw yet.".to_string()
    } else {
        format!("Used to read: {}.", t.used_for.join(", "))
    }
}

/// A found tool (or one whose check failed).
fn tool_row(t: ToolRow, expert: RwSignal<bool>) -> impl IntoView {
    // LibreOffice's parts are worth a newcomer's glance: which of Writer,
    // Calc and Impress there are.
    let parts = t
        .extras
        .iter()
        .find(|e| e.kind == ExtraKind::Components)
        .filter(|e| !e.present.is_empty())
        .map(|e| e.present.join(", "));
    let value = match (&t.version, parts) {
        (Some(v), Some(p)) => format!("{v} · {p}"),
        (Some(v), None) => v.clone(),
        (None, _) => String::new(),
    };
    let failed = t.state == ToolState::ProbeFailed;
    let sub = if failed {
        "Installed, but it did not answer lmgw's check, so lmgw doesn't use it. Check again once \
         it is fixed."
            .to_string()
    } else {
        used_for(&t)
    };
    let mut detail: Vec<String> = Vec::new();
    if let Some(p) = &t.path {
        detail.push(p.clone());
    }
    detail.push(t.licence.clone());
    detail.extend(t.extras.iter().map(extra_said));
    if let Some(m) = &t.message {
        detail.push(m.clone());
    }
    view! {
        <div class="prow doc-row" data-tool=t.id.clone()>
            <div class="prow-text">
                <span class="prow-title">{t.label}</span>
                <span class="prow-sub" class:doc-warn=failed>{sub}</span>
            </div>
            <span class="prow-value mono">{value}</span>
            {expert_line(expert, detail.join(" · "))}
        </div>
    }
}

/// A missing tool: what it would give, and how to install it here.
fn missing_row(m: MissingRow, expert: RwSignal<bool>) -> impl IntoView {
    let gives = if m.gives.is_empty() {
        "Not used by lmgw yet, so installing it adds nothing for now.".to_string()
    } else {
        format!("Lets lmgw read: {}.", m.gives.join(", "))
    };
    let note = m.install.first().and_then(|h| h.note.clone());
    let hints = m.install.into_iter().map(install_view).collect_view();
    view! {
        <div class="prow doc-row" data-tool=m.tool.clone()>
            <div class="prow-text">
                <span class="prow-title">{m.label}</span>
                <span class="prow-sub">{gives}</span>
                {note.map(|n| view! { <span class="prow-sub">{n}</span> })}
            </div>
            {hints}
            {expert_line(expert, m.licence)}
        </div>
    }
}

/// One install hint: a command with its Copy button (several lines under
/// the row), a link, a sentence naming the tool for the system's package
/// manager, or why there is nothing to install yet.
fn install_view(h: InstallHint) -> AnyView {
    match h.kind {
        HintKind::Command => {
            let block = h.text.contains('\n');
            let text = h.text.clone();
            let toasts = use_toasts();
            let copy = move |_| {
                let text = text.clone();
                spawn_local(async move {
                    match lmgw_ui_kit::clipboard::write_text(&text).await {
                        Ok(()) => toasts.ok("Copied".to_string()),
                        Err(why) => toasts.err(why),
                    }
                });
            };
            if block {
                view! {
                    <div class="doc-cmd-block">
                        <pre class="cmd-lines">{h.text}</pre>
                        <button type="button" class="btn sm" on:click=copy>"Copy"</button>
                    </div>
                }
                .into_any()
            } else {
                view! {
                    <div class="prow-end doc-cmd">
                        <code class="doc-cmd-text">{h.text}</code>
                        <button type="button" class="btn sm" on:click=copy>"Copy"</button>
                    </div>
                }
                .into_any()
            }
        }
        HintKind::Link => view! {
            <div class="prow-end">
                <a href=h.text.clone() target="_blank" rel="noopener noreferrer">
                    "Installer"
                </a>
            </div>
        }
        .into_any(),
        HintKind::NotYet => view! { <span class="prow-sub doc-warn">{h.text}</span> }.into_any(),
        HintKind::Manual => view! { <span class="prow-sub">{h.text}</span> }.into_any(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_says_what_it_reads_or_that_it_is_not_used_yet() {
        let mut t = ToolRow {
            id: "pandoc".into(),
            label: "Pandoc".into(),
            state: ToolState::Found,
            version: Some("0.0.0-example".into()),
            path: Some("/opt/example/bin/pandoc".into()),
            extras: Vec::new(),
            message: None,
            licence: "GPL-2.0-or-later".into(),
            used_for: Vec::new(),
            checked_at: 0,
            not_confined: None,
        };
        assert_eq!(used_for(&t), "Not used by lmgw yet.");
        t.used_for = vec!["PDF".into()];
        assert_eq!(used_for(&t), "Used to read: PDF.");
    }

    #[test]
    fn extras_read_as_lists_with_what_is_missing() {
        let e = ToolExtra {
            kind: ExtraKind::AudioDecoders,
            present: vec!["aac".into(), "flac".into()],
            absent: vec!["eac3".into()],
        };
        assert_eq!(
            extra_said(&e),
            "audio decoders: aac, flac (not installed: eac3)"
        );
    }
}
