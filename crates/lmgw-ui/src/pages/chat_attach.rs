//! The composer's and the transcript's attachment chips (chat-complete
//! design §8): a monochrome icon per kind, name, size and what extraction
//! found (pages, sheets, `~12k tokens`, the transcript's alias), a Text |
//! Pages switch on a text-class PDF draft, and the one Send gate — the
//! server's per-draft `blockers` — with the viewer's header. The chip type,
//! the pure helpers and the components live here; `chat.rs` only calls them.

use leptos::prelude::*;
use serde_json::{json, Value};

use super::chat::{Attachment, ThreadDetail};
use crate::scope::Scope;
use crate::widgets::use_toasts;

/// A file picked, dropped or pasted into the composer: uploads the instant
/// it is picked, so what sits on screen is always what a Send would bind —
/// never a local-only file the server has not seen yet.
#[derive(Clone)]
pub(super) struct DraftChip {
    pub(super) key: u64,
    /// `None` until the upload answers.
    pub(super) id: RwSignal<Option<i64>>,
    /// `None` until the upload answers — the server sniffs it from the bytes.
    pub(super) kind: RwSignal<Option<String>>,
    pub(super) name: RwSignal<String>,
    pub(super) size: RwSignal<Option<i64>>,
    pub(super) uploading: RwSignal<bool>,
    /// The upload's `ApiError` message, kept on the chip verbatim.
    pub(super) error: RwSignal<Option<String>>,
    /// Everything the server said about the file: extraction meta, the PDF
    /// mode and `blockers` for the thread's current model.
    pub(super) att: RwSignal<Attachment>,
}

pub(super) fn new_draft(key: u64, name: String, size: Option<i64>) -> DraftChip {
    DraftChip {
        key,
        id: RwSignal::new(None),
        kind: RwSignal::new(None),
        name: RwSignal::new(name),
        size: RwSignal::new(size),
        uploading: RwSignal::new(true),
        error: RwSignal::new(None),
        att: RwSignal::new(Attachment::default()),
    }
}

/// A stored draft attachment, reopened with its thread.
pub(super) fn draft_from_attachment(key: u64, a: Attachment) -> DraftChip {
    DraftChip {
        key,
        id: RwSignal::new(Some(a.id)),
        kind: RwSignal::new(Some(a.kind.clone())),
        name: RwSignal::new(a.name.clone()),
        size: RwSignal::new(Some(a.size)),
        uploading: RwSignal::new(false),
        error: RwSignal::new(None),
        att: RwSignal::new(a),
    }
}

impl DraftChip {
    /// The upload answered: every signal the composer reads is filled.
    pub(super) fn uploaded(&self, a: Attachment) {
        self.id.set(Some(a.id));
        self.kind.set(Some(a.kind.clone()));
        self.size.set(Some(a.size));
        self.att.set(a);
        self.uploading.set(false);
    }

    /// The chip as the transcript's optimistic user bubble shows it.
    pub(super) fn as_attachment(&self) -> Attachment {
        Attachment {
            id: self.id.get_untracked().unwrap_or_default(),
            kind: self.kind.get_untracked().unwrap_or_default(),
            name: self.name.get_untracked(),
            size: self.size.get_untracked().unwrap_or_default(),
            ..self.att.get_untracked()
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// `950`, `1.5k`, `12k`, `1.2M` — an estimate, so no false precision.
pub(super) fn fmt_tokens(n: i64) -> String {
    let n = n.max(0);
    let trim = |x: f64, unit: &str| {
        let s = format!("{x:.1}");
        format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
    };
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        trim(n as f64 / 1_000.0, "k")
    } else if n < 999_500 {
        format!("{}k", (n as f64 / 1_000.0).round() as i64)
    } else {
        trim(n as f64 / 1_000_000.0, "M")
    }
}

/// One line of a chip's detail: the text and whether it is a warning.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Fact {
    pub(super) text: String,
    pub(super) warn: bool,
}

fn fact(text: impl Into<String>) -> Fact {
    Fact {
        text: text.into(),
        warn: false,
    }
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A text-class PDF (every page has text) is the only one with a choice.
pub(super) fn is_text_pdf(a: &Attachment) -> bool {
    a.kind == "pdf" && a.meta["class"].as_str() == Some("text")
}

/// What extraction found, chip-sized: pages and scan state, format and
/// sheets, the token estimate, the transcript's origin or its failure.
pub(super) fn chip_facts(a: &Attachment) -> Vec<Fact> {
    let mut out = Vec::new();
    let m = &a.meta;
    match a.kind.as_str() {
        "pdf" => {
            let pages = m["pages"].as_i64();
            if let Some(p) = pages {
                out.push(fact(plural(p, "page", "pages")));
            }
            match m["class"].as_str() {
                Some("scanned") => out.push(Fact {
                    text: "scanned".into(),
                    warn: false,
                }),
                Some("hybrid") => {
                    let k = m["textless"].as_array().map_or(0, Vec::len);
                    let of = pages.map_or(String::new(), |p| format!(" of {p}"));
                    out.push(fact(format!("{k}{of} pages without text")));
                }
                _ => {}
            }
        }
        "office" => {
            if let Some(f) = m["format"].as_str() {
                out.push(fact(f));
            }
            if let Some(s) = m["sheets"].as_array().filter(|s| !s.is_empty()) {
                out.push(fact(plural(s.len() as i64, "sheet", "sheets")));
            }
        }
        "audio" => {
            if let Some(f) = m["format"].as_str() {
                out.push(fact(f));
            }
            if let Some(alias) = m["transcript_alias"].as_str() {
                out.push(fact(format!("transcript by {alias}")));
            } else if let Some(e) = m["transcript_error"].as_str() {
                out.push(Fact {
                    text: e.to_string(),
                    warn: true,
                });
            }
        }
        _ => {}
    }
    if a.kind != "image" {
        if let Some(t) = a.extracted_tokens.or_else(|| m["tokens"].as_i64()) {
            out.push(fact(format!("~{} tokens", fmt_tokens(t))));
        }
    }
    out
}

/// An audio draft whose transcription failed: the one chip with a retry.
pub(super) fn can_retry_transcript(a: &Attachment) -> bool {
    a.kind == "audio" && a.meta["transcript_error"].as_str().is_some()
}

/// The label of a PDF's mode on a sent chip.
pub(super) fn mode_label(mode: Option<&str>) -> Option<&'static str> {
    match mode {
        Some("text") => Some("as text"),
        Some("images") => Some("as page images"),
        _ => None,
    }
}

/// Why a Send would be refused: the server's blockers when a draft has them
/// (they cover images, PDFs and audio for the thread's current model), and
/// only for a draft the server has not answered for yet — a fresh upload
/// before its thread is re-read — the local vision check. Deduplicated, in
/// chip order.
pub(super) fn select_blockers(drafts: &[Attachment], vision_no: bool, model: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    for a in drafts {
        match &a.blockers {
            Some(b) => b.iter().cloned().for_each(&mut add),
            None if a.kind == "image" && vision_no => add(vision_block_reason(model)),
            None => {}
        }
    }
    out
}

/// The local fallback wording — the server's own says the same thing
/// (`chat_attach_gate::blockers`).
pub(super) fn vision_block_reason(model: &str) -> String {
    format!("'{model}' does not accept images — remove the image or switch models")
}

/// What the drafts become on the way, sent all the same: the server's
/// `hints` (a GPU block's fallback that cannot see gets an image as a
/// placeholder, a PDF's pages as its text). Deduplicated, in chip order.
pub(super) fn select_hints(drafts: &[Attachment]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for h in drafts.iter().flat_map(|a| a.hints.iter().flatten()) {
        if !out.contains(h) {
            out.push(h.clone());
        }
    }
    out
}

/// The composer's hints, tracked: read inside a `Signal::derive`.
pub(super) fn draft_hints(chips: &[DraftChip]) -> Vec<String> {
    let drafts: Vec<Attachment> = chips
        .iter()
        .filter(|c| c.id.get().is_some() && c.error.get().is_none())
        .map(|c| c.att.get())
        .collect();
    select_hints(&drafts)
}

/// The composer's blockers, tracked: read inside a `Signal::derive`.
pub(super) fn draft_blockers(chips: &[DraftChip], vision_no: bool, model: &str) -> Vec<String> {
    let drafts: Vec<Attachment> = chips
        .iter()
        .filter(|c| c.id.get().is_some() && c.error.get().is_none())
        .map(|c| c.att.get())
        .collect();
    select_blockers(&drafts, vision_no, model)
}

/// Re-read the thread's drafts and fold the answer into the chips: the
/// server computes `blockers` against the model as it is now, and neither
/// the mode route nor the settings route returns them.
pub(super) fn refresh_drafts(
    chips: RwSignal<Vec<DraftChip>>,
    scope: Scope,
    tid: i64,
    is_current: impl Fn(i64) -> bool + 'static,
) {
    scope.spawn(async move {
        let Ok(d) = crate::api::get::<ThreadDetail>(format!("/chat/api/threads/{tid}")).await
        else {
            return;
        };
        if !is_current(tid) {
            return;
        }
        fold_drafts(chips, &d.draft_attachments);
    });
}

/// The thread's drafts as the server read them (`fresh`) folded into the
/// chips that are them, where they differ: their `blockers` and `hints`
/// against the thread's model as stored, their mode. A chip still
/// uploading, or one the server does not list, is left as it is.
pub(super) fn fold_drafts(chips: RwSignal<Vec<DraftChip>>, fresh: &[Attachment]) {
    chips.with_untracked(|v| {
        for c in v {
            if let Some(a) = fresh.iter().find(|a| Some(a.id) == c.id.get_untracked()) {
                if c.att.with_untracked(|cur| cur != a) {
                    c.att.set(a.clone());
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

/// A monochrome line icon for a kind (`image` shows its thumbnail instead).
fn kind_icon(kind: &str) -> impl IntoView {
    let path = match kind {
        "pdf" => "M4 1.5h5.5L13 5v9.5H4zM9.5 1.5V5H13M6 9h5M6 11.5h5",
        "office" => "M2.5 2.5h11v11h-11zM2.5 6.5h11M6.5 6.5v7",
        "audio" => "M2 6v4M5 4v8M8 2v12M11 5v6M14 7v2",
        _ => "M4 1.5h8v13H4zM6 5h4M6 8h4M6 11h2.5",
    };
    view! {
        <svg class="attach-ico" viewBox="0 0 16 16" aria-hidden="true">
            <path d=path/>
        </svg>
    }
}

/// The detail lines after the name.
#[component]
fn Facts(#[prop(into)] facts: Signal<Vec<Fact>>) -> impl IntoView {
    view! {
        <For each=move || facts.get() key=|f| f.text.clone() let:f>
            <span class="chip-fact" class:warn=f.warn>{f.text}</span>
        </For>
    }
}

/// One sent message's attachment chip: a thumbnail or kind icon, the name,
/// its size and what extraction found, and a click that opens the full
/// thing in the shared viewer. A PDF says how it went (read-only). Carries a
/// warning mark when it is an image (or page images) and the thread's
/// current model will not take it (design §3).
#[component]
pub(super) fn SentChip(
    a: Attachment,
    #[prop(into)] vision_no: Signal<bool>,
    on_open: Callback<Attachment>,
) -> impl IntoView {
    let is_image = a.kind == "image";
    let pages_mode = a.kind == "pdf" && a.mode.as_deref() == Some("images");
    let warn = Signal::derive(move || (is_image || pages_mode) && vision_no.get());
    let mut facts = chip_facts(&a);
    if a.kind == "pdf" {
        if let Some(l) = mode_label(a.mode.as_deref()) {
            facts.insert(1.min(facts.len()), fact(l));
        }
    }
    let click_a = a.clone();
    view! {
        <span
            class="chip attach-chip rich"
            class:warn=warn
            role="button"
            tabindex="0"
            title=if is_image { "Open image" } else { "Open extracted text" }
            on:click=move |_| on_open.run(click_a.clone())
        >
            {if is_image {
                view! { <img class="chip-thumb" src=format!("/chat/api/attachments/{}", a.id)/> }
                    .into_any()
            } else {
                kind_icon(&a.kind).into_any()
            }}
            <span
                class="chip-name"
                title=format!("{} ({})", a.name, crate::fmt::human_bytes(a.size.max(0) as u64))
            >
                {a.name.clone()}
            </span>
            <span class="dim">{crate::fmt::human_bytes(a.size.max(0) as u64)}</span>
            <Facts facts=facts/>
            <Show when=move || warn.get()>
                <span class="chip-warn-mark" title="not sent to this model">"⚠"</span>
            </Show>
        </span>
    }
}

/// The composer's draft chips: uploading, uploaded (thumbnail or icon, name,
/// size, facts, ✕) or failed (the server's own message, kept verbatim, and ✕).
#[component]
pub(super) fn DraftChips(
    chips: RwSignal<Vec<DraftChip>>,
    #[prop(into)] vision_no: Signal<bool>,
    on_remove: Callback<DraftChip>,
    on_open: Callback<Attachment>,
    /// Re-read the drafts' blockers (after a mode change).
    on_refresh: Callback<()>,
) -> impl IntoView {
    view! {
        <Show when=move || !chips.with(Vec::is_empty)>
            <div class="chip-row draft-chips">
                <For each=move || chips.get() key=|c| c.key let:chip>
                    <DraftChipView
                        chip=chip
                        vision_no=vision_no
                        on_remove=on_remove
                        on_open=on_open
                        on_refresh=on_refresh
                    />
                </For>
            </div>
        </Show>
    }
}

#[component]
fn DraftChipView(
    chip: DraftChip,
    #[prop(into)] vision_no: Signal<bool>,
    on_remove: Callback<DraftChip>,
    on_open: Callback<Attachment>,
    on_refresh: Callback<()>,
) -> impl IntoView {
    let toasts = use_toasts();
    let id = chip.id;
    let kind = chip.kind;
    let name = chip.name;
    let size = chip.size;
    let uploading = chip.uploading;
    let error = chip.error;
    let att = chip.att;
    let is_image = Signal::derive(move || kind.get().as_deref() == Some("image"));
    let blocked = Signal::derive(move || {
        att.with(|a| a.blockers.as_ref().is_some_and(|b| !b.is_empty()))
            || (is_image.get() && vision_no.get() && att.with(|a| a.blockers.is_none()))
    });
    let facts = Signal::derive(move || att.with(chip_facts));
    let remove_chip = chip.clone();
    let text_pdf = Signal::derive(move || att.with(is_text_pdf));
    let mode = Signal::derive(move || att.with(|a| a.mode.clone()));
    let open_chip = chip.clone();
    let retryable = Signal::derive(move || att.with(can_retry_transcript));
    let retrying = RwSignal::new(false);

    // The transcription is asked for again; the chip takes the new meta and
    // the blockers are re-read (the blocker text says "retry the
    // transcription or remove the file").
    let retry = move |_| {
        let Some(aid) = id.get_untracked() else {
            return;
        };
        if retrying.get_untracked() {
            return;
        }
        retrying.set(true);
        let scope = Scope::new();
        scope.spawn(async move {
            let res = crate::api::post::<Value, _>(
                format!("/chat/api/attachments/{aid}/transcribe"),
                &json!({}),
            )
            .await;
            retrying.try_set(false);
            match res {
                Ok(v) => {
                    att.try_update(|a| a.meta = v["meta"].clone());
                    on_refresh.run(());
                }
                Err(e) => toasts.err(format!("transcription failed: {e}")),
            }
        });
    };

    // Optimistic: the switch moves at once, is put back with a toast when the
    // server refuses, and the blockers are re-read either way it succeeds.
    let set_mode = move |m: &'static str| {
        let Some(aid) = id.get_untracked() else {
            return;
        };
        let prev = mode.get_untracked();
        if prev.as_deref() == Some(m) {
            return;
        }
        att.update(|a| a.mode = Some(m.to_string()));
        let scope = Scope::new();
        scope.spawn(async move {
            match crate::api::post::<Value, _>(
                format!("/chat/api/attachments/{aid}/mode"),
                &json!({ "mode": m }),
            )
            .await
            {
                Ok(_) => on_refresh.run(()),
                Err(e) => {
                    att.try_update(|a| a.mode = prev);
                    toasts.err(format!("setting the PDF mode failed: {e}"));
                }
            }
        });
    };
    let seg = move |m: &'static str, label: &'static str, hint: &'static str| {
        view! {
            <button
                type="button"
                class="seg-btn"
                class:on=move || mode.get().as_deref() == Some(m)
                title=hint
                on:click=move |_| set_mode(m)
            >
                {label}
            </button>
        }
    };
    view! {
        <span
            class="chip attach-chip rich"
            class:err=move || error.get().is_some()
            class:warn=blocked
        >
            {move || {
                if uploading.get() {
                    view! { <span class="chip-dot-pending" title="Uploading…"></span> }.into_any()
                } else if is_image.get() {
                    id.get()
                        .map(|i| {
                            view! { <img class="chip-thumb" src=format!("/chat/api/attachments/{i}")/> }
                        })
                        .into_any()
                } else if error.get().is_none() {
                    kind_icon(&kind.get().unwrap_or_default()).into_any()
                } else {
                    ().into_any()
                }
            }}
            <span
                class="chip-name"
                class:chip-open=move || id.get().is_some()
                role="button"
                tabindex="0"
                title=move || {
                    let n = name.get();
                    match size.get() {
                        Some(s) => format!("{n} ({}) — open", crate::fmt::human_bytes(s.max(0) as u64)),
                        None => n,
                    }
                }
                on:click=move |_| {
                    if id.get_untracked().is_some() && error.get_untracked().is_none() {
                        on_open.run(open_chip.as_attachment());
                    }
                }
            >
                {move || name.get()}
            </span>
            {move || {
                if let Some(e) = error.get() {
                    view! { <span class="chip-err-text">{e}</span> }.into_any()
                } else if uploading.get() {
                    view! { <span class="dim">"uploading…"</span> }.into_any()
                } else {
                    view! {
                        <span class="dim">
                            {size.get().map(|s| crate::fmt::human_bytes(s.max(0) as u64))}
                        </span>
                        <Facts facts=facts/>
                    }
                        .into_any()
                }
            }}
            <Show when=move || text_pdf.get() && !uploading.get()>
                <span class="seg-mini" role="group" title="How this PDF goes to the model">
                    {seg("text", "Text", "Send the extracted text")}
                    {seg("images", "Pages", "Send every page as an image (needs a vision model)")}
                </span>
            </Show>
            <Show when=move || retryable.get() && !uploading.get()>
                <button
                    type="button"
                    class="btn ghost sm"
                    title="Transcribe this file again"
                    disabled=move || retrying.get()
                    on:click=retry
                >
                    {move || if retrying.get() { "Transcribing…" } else { "Retry transcription" }}
                </button>
            </Show>
            <Show when=move || blocked.get()>
                <span class="chip-warn-mark" title="this file cannot be sent yet — see below">
                    "⚠"
                </span>
            </Show>
            <button
                type="button"
                class="chip-x"
                title="Remove"
                on:click=move |_| on_remove.run(remove_chip.clone())
            >
                "✕"
            </button>
        </span>
    }
}

/// What the drafts become on the way, under the chip row — amber like the
/// GPU hold's other notes, and never blocking Send.
#[component]
pub(super) fn HintNote(#[prop(into)] hints: Signal<Vec<String>>) -> impl IntoView {
    view! {
        <Show when=move || !hints.with(Vec::is_empty)>
            <div class="chip-row attach-hints">
                <For each=move || hints.get() key=|h| h.clone() let:h>
                    <div class="mini-note attach-hint">{h}</div>
                </For>
            </div>
        </Show>
    }
}

/// Why Send is disabled, under the chip row — one line per reason.
#[component]
pub(super) fn BlockerNote(#[prop(into)] blockers: Signal<Vec<String>>) -> impl IntoView {
    view! {
        <Show when=move || !blockers.with(Vec::is_empty)>
            <div class="chip-row attach-blockers">
                <For each=move || blockers.get() key=|b| b.clone() let:b>
                    <div class="mini-note attach-blocker">{b}</div>
                </For>
            </div>
        </Show>
    }
}

/// The viewer's header for a non-image attachment: the token estimate and
/// Download original (WebKitGTK has no inline PDF viewer, so the original is
/// only ever downloaded).
#[component]
pub(super) fn ViewerMeta(att: RwSignal<Option<Attachment>>) -> impl IntoView {
    view! {
        {move || {
            att.get()
                .filter(|a| a.kind != "image")
                .map(|a| {
                    let tokens = a.extracted_tokens.or_else(|| a.meta["tokens"].as_i64());
                    let href = format!("/chat/api/attachments/{}", a.id);
                    let file = a.name.clone();
                    view! {
                        <div class="viewer-meta">
                            <span class="dim mono-sm">
                                {tokens.map(|t| format!("~{} tokens", fmt_tokens(t)))}
                            </span>
                            <button
                                type="button"
                                class="btn ghost sm"
                                on:click=move |_| super::docs::download(&href, &file)
                            >
                                "Download original"
                            </button>
                        </div>
                    }
                })
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(kind: &str, meta: Value) -> Attachment {
        Attachment {
            id: 1,
            kind: kind.into(),
            name: "f".into(),
            meta,
            ..Default::default()
        }
    }

    #[test]
    fn tokens_read_like_an_estimate() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(1_000), "1k");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(12_345), "12k");
        assert_eq!(fmt_tokens(999_999), "1M");
        assert_eq!(fmt_tokens(1_240_000), "1.2M");
        assert_eq!(fmt_tokens(-4), "0");
    }

    fn texts(a: &Attachment) -> Vec<String> {
        chip_facts(a).into_iter().map(|f| f.text).collect()
    }

    #[test]
    fn pdf_facts_by_class() {
        let mut a = att(
            "pdf",
            json!({"pages": 12, "class": "text", "textless": [], "tokens": 12000}),
        );
        a.extracted_tokens = Some(12_000);
        assert_eq!(texts(&a), ["12 pages", "~12k tokens"]);
        assert!(is_text_pdf(&a));
        let s = att(
            "pdf",
            json!({"pages": 1, "class": "scanned", "textless": [1], "tokens": 0}),
        );
        assert_eq!(texts(&s), ["1 page", "scanned", "~0 tokens"]);
        assert!(!is_text_pdf(&s));
        let h = att(
            "pdf",
            json!({"pages": 9, "class": "hybrid", "textless": [3, 7], "tokens": 950}),
        );
        assert_eq!(
            texts(&h),
            ["9 pages", "2 of 9 pages without text", "~950 tokens"]
        );
    }

    #[test]
    fn office_audio_image_facts() {
        let o = att(
            "office",
            json!({"format": "xlsx", "parts": 2, "sheets": ["a", "b"], "tokens": 1500}),
        );
        assert_eq!(texts(&o), ["xlsx", "2 sheets", "~1.5k tokens"]);
        let d = att(
            "office",
            json!({"format": "docx", "parts": 1, "tokens": 40}),
        );
        assert_eq!(texts(&d), ["docx", "~40 tokens"]);
        let a = att(
            "audio",
            json!({"format": "wav", "transcript_alias": "whisper", "tokens": 300}),
        );
        assert_eq!(texts(&a), ["wav", "transcript by whisper", "~300 tokens"]);
        let e = att(
            "audio",
            json!({"format": "mp3", "transcript_error": "stt down"}),
        );
        let f = chip_facts(&e);
        assert_eq!(f[1].text, "stt down");
        assert!(f[1].warn);
        assert!(chip_facts(&att("image", json!({}))).is_empty());
        assert_eq!(texts(&att("text", json!({"tokens": 3}))), ["~3 tokens"]);
    }

    #[test]
    fn server_blockers_win_and_the_local_check_is_a_fallback() {
        let mut img = att("image", json!({}));
        // no server answer yet: the local vision check
        assert_eq!(
            select_blockers(&[img.clone()], true, "m"),
            [vision_block_reason("m")]
        );
        assert!(select_blockers(&[img.clone()], false, "m").is_empty());
        // the server answered "fine": the local check stays quiet
        img.blockers = Some(vec![]);
        assert!(select_blockers(&[img.clone()], true, "m").is_empty());
        // several drafts, duplicates collapsed, chip order kept
        let mut p = att("pdf", json!({"class": "text"}));
        p.blockers = Some(vec!["choose Text or Pages for f".into()]);
        let mut q = p.clone();
        q.blockers = Some(vec!["choose Text or Pages for f".into(), "second".into()]);
        assert_eq!(
            select_blockers(&[p, q], false, "m"),
            ["choose Text or Pages for f", "second"]
        );
    }

    #[test]
    fn only_a_failed_audio_transcript_can_be_retried() {
        assert!(can_retry_transcript(&att(
            "audio",
            json!({"transcript_error": "stt down"})
        )));
        assert!(!can_retry_transcript(&att(
            "audio",
            json!({"transcript_alias": "whisper"})
        )));
        assert!(!can_retry_transcript(&att(
            "pdf",
            json!({"transcript_error": "x"})
        )));
    }

    #[test]
    fn sent_pdf_mode_labels() {
        assert_eq!(mode_label(Some("text")), Some("as text"));
        assert_eq!(mode_label(Some("images")), Some("as page images"));
        assert_eq!(mode_label(None), None);
    }
}
