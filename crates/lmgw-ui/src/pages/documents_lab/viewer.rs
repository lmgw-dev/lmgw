//! One file's result (WP12): its status, how it was read (`stats`), its
//! warnings with their codes in plain words, and its text in each form the
//! answer has: the markdown rendered, the text as it is, the structure as an
//! outline and as its JSON, each with Copy and Save. A kept text is fetched
//! from the lab store when a form that shows it is first opened, and held
//! (with the structure's parse) for the file's view, so a switch between
//! forms neither fetches nor parses again (Markdown and Raw are one text;
//! Structure and JSON are one); a long one is shown a page at a time
//! ([`super::pager`]).

use std::collections::HashMap;
use std::sync::Arc;

use leptos::prelude::*;
use lmgw_api_types::documents::convert::{ConvertStats, ConvertWarning};
use lmgw_api_types::documents::structure::DocPart;

use super::outline::Outline;
use super::pager::{page_count, pages};
use super::rendered::Rendered;
use super::result::{FileResult, Text};
use super::words;
use crate::fmt::{fmt_bytes, grouped};
use crate::widgets::use_toasts;

/// Steps through `count` pages: "‹ 3 of 41 ›".
#[component]
pub(super) fn Pager(
    page: RwSignal<usize>,
    #[prop(into)] count: Signal<usize>,
    /// What the pages hold, for the tooltip.
    what: &'static str,
) -> impl IntoView {
    let last = move || count.get().saturating_sub(1);
    view! {
        <span class="doclab-pager" hidden={move || count.get() <= 1}>
            <button
                class="btn ghost sm"
                title=format!("The previous page of {what}")
                disabled=move || page.get() == 0
                on:click=move |_| page.update(|p| *p = p.saturating_sub(1))
            >
                "‹"
            </button>
            <span class="mono-sm">{move || format!("{} of {}", page.get() + 1, count.get())}</span>
            <button
                class="btn ghost sm"
                title=format!("The next page of {what}")
                disabled={move || page.get() >= last()}
                on:click=move |_| page.update(|p| *p = (*p + 1).min(last()))
            >
                "›"
            </button>
        </span>
    }
}

/// Which form of a file's text is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Form {
    Rendered,
    Text,
    Plain,
    Outline,
    Json,
}

impl Form {
    fn label(self) -> &'static str {
        match self {
            Self::Rendered => "Markdown",
            Self::Text => "Raw",
            Self::Plain => "Text",
            Self::Outline => "Structure",
            Self::Json => "JSON",
        }
    }
}

/// The forms `f` has, in the order the switch lists them.
pub(super) fn forms(f: &FileResult) -> Vec<Form> {
    let mut out = Vec::new();
    if f.content.is_some() {
        if f.markdown {
            out.push(Form::Rendered);
        }
        out.push(Form::Text);
    }
    if f.text.is_some() {
        out.push(Form::Plain);
    }
    if f.structure.is_some() {
        // docling's own document is no list of lmgw's parts.
        if !f.structure_is_docling {
            out.push(Form::Outline);
        }
        out.push(Form::Json);
    }
    out
}

/// Which of a file's three texts a form shows, and the text.
fn text_for(f: &FileResult, form: Form) -> Option<(&'static str, Text)> {
    match form {
        Form::Rendered | Form::Text => f.content.clone().map(|t| ("content", t)),
        Form::Plain => f.text.clone().map(|t| ("text", t)),
        Form::Outline | Form::Json => f.structure.clone().map(|t| ("structure", t)),
    }
}

type Parsed = Result<Arc<Vec<DocPart>>, String>;

/// What a file's view holds across its forms: each text's load state and the
/// structure's parse, by the text's name.
#[derive(Clone, Copy)]
struct Held {
    /// The file view's owner: a state made while a form is shown must
    /// outlive that form's scope, or the next form reads a disposed signal.
    owner: StoredValue<Owner>,
    loads: StoredValue<HashMap<&'static str, RwSignal<Loaded>>>,
    parsed: StoredValue<HashMap<&'static str, Parsed>>,
}

impl Held {
    fn new() -> Self {
        Self {
            owner: StoredValue::new(Owner::current().expect("a file view has an owner")),
            loads: StoredValue::new(HashMap::new()),
            parsed: StoredValue::new(HashMap::new()),
        }
    }

    /// The text's load state: the one held, or a new one whose fetch starts
    /// now.
    fn load(&self, tag: &'static str, t: &Text) -> RwSignal<Loaded> {
        if let Some(l) = self.loads.with_value(|m| m.get(tag).copied()) {
            return l;
        }
        let loaded = self
            .owner
            .get_value()
            .with(|| RwSignal::new(Loaded::Loading));
        self.loads.update_value(|m| {
            m.insert(tag, loaded);
        });
        let t = t.clone();
        leptos::task::spawn_local(async move {
            let got = fetch(t).await;
            loaded.try_set(got);
        });
        loaded
    }

    /// The structure parsed, once per text.
    fn parse(&self, tag: &'static str, text: &str) -> Parsed {
        if let Some(p) = self.parsed.with_value(|m| m.get(tag).cloned()) {
            return p;
        }
        let p: Parsed = serde_json::from_str::<Vec<DocPart>>(text)
            .map(Arc::new)
            .map_err(|e| e.to_string());
        self.parsed.update_value(|m| {
            m.insert(tag, p.clone());
        });
        p
    }
}

/// How a file was read, in a line: "12 parts: 11 own text, 1 text
/// recognition · headings: 3 at level 1, 9 at level 2 · 2 tables (48
/// cells) · 840 ms".
pub(super) fn stats_line(parts: u32, s: &ConvertStats) -> String {
    let p = &s.parts_by_source;
    let by: Vec<String> = [
        (p.text_layer, "own text"),
        (p.native, "lmgw's reader"),
        (p.ocr, "text recognition"),
        (p.vision, "a model"),
        (p.asr, "a speech-to-text model"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, w)| format!("{n} {w}"))
    .collect();
    let parts_n = words::count(parts, "part", "parts");
    let mut out = vec![if by.is_empty() {
        parts_n
    } else {
        format!("{parts_n}: {}", by.join(", "))
    }];
    let heads: Vec<String> = s
        .headings
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(i, n)| format!("{n} at level {}", i + 1))
        .collect();
    if !heads.is_empty() {
        out.push(format!("headings: {}", heads.join(", ")));
    }
    if s.tables > 0 {
        out.push(format!(
            "{} ({})",
            words::count(s.tables, "table", "tables"),
            words::count(s.table_cells, "cell", "cells")
        ));
    }
    if s.pictures > 0 {
        let described: u32 = s.models.iter().map(|m| m.pictures_described).sum();
        let mut said = words::count(s.pictures, "picture", "pictures");
        if described > 0 {
            said.push_str(&format!(" ({described} described)"));
        }
        out.push(said);
    }
    if s.model_calls > 0 {
        out.push(words::count(s.model_calls, "model call", "model calls"));
    }
    out.push(format!("{} ms", grouped(s.time_ms)));
    out.join(" · ")
}

/// A warning's line: its words, its message, the parts it names.
#[component]
pub(super) fn WarningRow(w: ConvertWarning) -> impl IntoView {
    let code = serde_json::to_value(w.code)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let words = words::warning(&code);
    let strong = matches!(
        code.as_str(),
        "not_yet" | "tool_missing" | "model_not_chosen" | "model_held" | "model_not_allowed"
    );
    let extra = [
        w.option.map(|o| format!("option {o}")),
        w.tool.map(|t| format!("tool {t}")),
        (!w.parts.is_empty()).then(|| format!("{} parts", w.parts.len())),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    view! {
        <div class="doclab-warn" class:strong=strong>
            <span class="doclab-warn-head">
                <b>{words}</b>
                <span class="mono-sm dim">{code}</span>
                {(!extra.is_empty()).then(|| view! { <span class="dim">{extra}</span> })}
            </span>
            <span class="doclab-warn-msg">{w.message}</span>
        </div>
    }
}

/// A kept or inline text's state.
#[derive(Clone, Debug, PartialEq)]
enum Loaded {
    Loading,
    Ready(String),
    Failed(String),
}

async fn fetch(t: Text) -> Loaded {
    match t {
        Text::Inline(s) => Loaded::Ready(s),
        Text::Kept { href, .. } => {
            let sent = gloo_net::http::Request::get(&href).send().await;
            match sent {
                Ok(r) if r.ok() => match r.text().await {
                    Ok(s) => Loaded::Ready(s),
                    Err(e) => Loaded::Failed(e.to_string()),
                },
                Ok(r) => Loaded::Failed(format!("HTTP {}", r.status())),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        }
    }
}

fn save_name(f: &FileResult, form: Form, t: &Text) -> String {
    if let Text::Kept { name, .. } = t {
        return name.clone();
    }
    let stem = f.name.rsplit_once('.').map_or(f.name.as_str(), |(s, _)| s);
    let ext = match form {
        Form::Rendered | Form::Text if f.markdown => "md",
        Form::Outline | Form::Json => "structure.json",
        _ => "txt",
    };
    format!("{stem}.{ext}")
}

/// One form of the text, a page at a time.
#[component]
fn TextForm(
    f: FileResult,
    form: Form,
    tag: &'static str,
    t: Text,
    held: Held,
    #[prop(into)] expert: Signal<bool>,
) -> impl IntoView {
    let loaded = held.load(tag, &t);
    let toasts = use_toasts();
    let markdown = form == Form::Rendered;
    // The page ranges, once per text.
    let ranges = Memo::new(move |_| {
        loaded.with(|l| match l {
            Loaded::Ready(s) => pages(s, markdown),
            _ => Vec::new(),
        })
    });
    let page = RwSignal::new(0usize);
    let count = Signal::derive(move || ranges.with(|r| page_count(r.len(), 1)));
    let chunk = Signal::derive(move || {
        let p = page.get();
        ranges.with(|r| {
            r.get(p)
                .map(|r| {
                    loaded.with(|l| match l {
                        Loaded::Ready(s) => s.get(r.clone()).unwrap_or_default().to_string(),
                        _ => String::new(),
                    })
                })
                .unwrap_or_default()
        })
    });
    let parts = Memo::new(move |_| {
        if form != Form::Outline {
            return None;
        }
        loaded.with(|l| match l {
            Loaded::Ready(s) => Some(held.parse(tag, s)),
            _ => None,
        })
    });
    let copy = move |_| {
        let Loaded::Ready(s) = loaded.get_untracked() else {
            return;
        };
        leptos::task::spawn_local(async move {
            match lmgw_ui_kit::clipboard::write_text(&s).await {
                Ok(()) => toasts.ok(format!("copied ({})", fmt_bytes(s.len() as u64))),
                Err(e) => toasts.err(format!("Not copied: {e}.")),
            }
        });
    };
    let name = save_name(&f, form, &t);
    let save = {
        let t = t.clone();
        let name = name.clone();
        move |_| match &t {
            Text::Kept { href, .. } => crate::pages::docs::download(href, &name),
            Text::Inline(s) => {
                let href = format!(
                    "data:text/plain;charset=utf-8,{}",
                    js_sys::encode_uri_component(s)
                );
                crate::pages::docs::download(&href, &name);
            }
        }
    };
    let size = fmt_bytes(t.size());
    view! {
        <div class="doclab-form-head">
            <span class="dim mono-sm" title=name.clone()>{format!("{name} · {size}")}</span>
            <span class="spacer"></span>
            {(form != Form::Outline)
                .then(|| view! { <Pager page=page count=count what="the text"/> })}
            <button class="btn ghost sm" title="Copy this form of the text whole" on:click=copy>
                "Copy"
            </button>
            <button class="btn ghost sm" title="Save this form of the text as a file" on:click=save>
                "Save…"
            </button>
        </div>
        <div class="doclab-text">
            {move || match loaded.get() {
                Loaded::Loading => view! { <div class="dim">"Reading the text…"</div> }.into_any(),
                Loaded::Failed(e) => {
                    view! { <div class="notice err">{format!("The text did not load: {e}")}</div> }
                        .into_any()
                }
                Loaded::Ready(_) => match form {
                    Form::Rendered => view! { <Rendered markdown=chunk/> }.into_any(),
                    Form::Outline => match parts.get() {
                        Some(p) => match p {
                            Ok(p) => view! { <Outline parts=p expert=expert/> }.into_any(),
                            Err(e) => {
                                view! { <div class="notice err">{format!("The structure does not read: {e}")}</div> }
                                    .into_any()
                            }
                        },
                        None => ().into_any(),
                    },
                    _ => view! { <pre class="doclab-raw">{move || chunk.get()}</pre> }.into_any(),
                },
            }}
        </div>
    }
}

/// One file's result.
#[component]
pub(super) fn FileView(f: FileResult, #[prop(into)] expert: Signal<bool>) -> impl IntoView {
    let all = forms(&f);
    let form = RwSignal::new(all.first().copied().unwrap_or(Form::Text));
    let chip = words::status_chip(&f.status);
    let state = words::state(&f.status);
    let facts = [
        f.format.clone(),
        (f.parts > 0).then(|| words::count(f.parts, "part", "parts")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let stats = f.stats.as_ref().map(|s| stats_line(f.parts, s));
    let error = f.error.clone();
    let warnings = f.warnings.clone();
    let errors = f.errors.clone();
    let name = f.name.clone();
    let switch = (all.len() > 1).then(|| {
        let all = all.clone();
        view! {
            <div class="seg doclab-forms">
                {all
                    .into_iter()
                    .map(|k| {
                        view! {
                            <button
                                type="button"
                                class="seg-btn"
                                class:active=move || form.get() == k
                                on:click=move |_| form.set(k)
                            >
                                {k.label()}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
        }
    });
    let held = Held::new();
    let body = {
        let f = f.clone();
        move || {
            let k = form.get();
            text_for(&f, k).map(|(tag, t)| {
                view! { <TextForm f=f.clone() form=k tag=tag t=t held=held expert=expert/> }
            })
        }
    };
    view! {
        <div class="doclab-file">
            <div class="doclab-file-head">
                <span class="doclab-file-name" title=name.clone()>{name.clone()}</span>
                <span class=chip>{state}</span>
                {(!facts.is_empty()).then(|| view! { <span class="dim mono-sm">{facts}</span> })}
            </div>
            {error
                .map(|(code, msg)| {
                    view! {
                        <div class="notice err doclab-error">
                            <b>{words::error(&code)}</b>
                            " "
                            <span class="mono-sm">{code}</span>
                            <div>{msg}</div>
                        </div>
                    }
                })}
            {stats.map(|s| view! { <div class="doclab-stats mono-sm dim">{s}</div> })}
            {(!warnings.is_empty() || !errors.is_empty())
                .then(|| {
                    view! {
                        <div class="doclab-warns">
                            {warnings.into_iter().map(|w| view! { <WarningRow w=w/> }).collect_view()}
                            {errors
                                .into_iter()
                                .map(|e| view! { <div class="doclab-warn"><span class="doclab-warn-msg">{e}</span></div> })
                                .collect_view()}
                        </div>
                    }
                })}
            {switch}
            {body}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents::convert::PartsBySource;

    #[test]
    fn the_stats_line_says_how_the_parts_were_read() {
        let s = ConvertStats {
            parts_by_source: PartsBySource {
                text_layer: 11,
                ocr: 1,
                vision: 0,
                native: 0,
                asr: 0,
            },
            headings: [3, 9, 0, 0, 0, 0],
            tables: 2,
            table_cells: 48,
            pictures: 0,
            pictures_below_describe_min_area: 0,
            model_calls: 0,
            models: Vec::new(),
            time_ms: 840,
        };
        assert_eq!(
            stats_line(12, &s),
            "12 parts: 11 own text, 1 text recognition · headings: 3 at level 1, 9 at level 2 · 2 tables (48 cells) · 840 ms"
        );
    }

    #[test]
    fn a_file_shows_the_forms_its_answer_has() {
        let md = FileResult {
            content: Some(Text::Inline("# a".into())),
            markdown: true,
            ..Default::default()
        };
        assert_eq!(forms(&md), [Form::Rendered, Form::Text]);
        let json = FileResult {
            structure: Some(Text::Inline("[]".into())),
            ..Default::default()
        };
        assert_eq!(forms(&json), [Form::Outline, Form::Json]);
        let docling = FileResult {
            structure: Some(Text::Inline("{}".into())),
            structure_is_docling: true,
            ..Default::default()
        };
        assert_eq!(
            forms(&docling),
            [Form::Json],
            "docling's json has no outline"
        );
        assert!(forms(&FileResult::default()).is_empty());
    }
}
