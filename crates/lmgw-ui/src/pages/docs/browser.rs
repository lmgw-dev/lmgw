//! The corpus browser: a corpus's documents under their folders (the
//! shared prefix shown once) beside the chunks of the one picked, each
//! chunk's verbatim payload with its derived title and summary labelled as
//! such (quickdoc §11).

use leptos::prelude::*;
use lmgw_api_types::{ChunksResponse, CorpusView, DocumentRow, DocumentsResponse};

use crate::fmt::grouped;
use crate::widgets::{FilterBar, Modal, ModalSize, Side, SplitPane};

use super::use_docs;

// ---------------------------------------------------------------------------

/// The part every document URL shares, cut back to a `/` so it is a
/// directory: shown once over the list, which then reads as the paths under
/// it. One document shares its own directory; none share nothing.
pub fn common_prefix<'a>(urls: impl IntoIterator<Item = &'a str>) -> String {
    let mut it = urls.into_iter();
    let Some(first) = it.next() else {
        return String::new();
    };
    let mut len = first.len();
    for u in it {
        let same = first
            .bytes()
            .zip(u.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        len = len.min(same);
    }
    while !first.is_char_boundary(len) {
        len -= 1;
    }
    let shared = &first[..len];
    match shared.rfind('/') {
        Some(i) => shared[..=i].to_string(),
        None => String::new(),
    }
}

/// A document's name in the browser: its path under the folder. A URL that
/// ends in `/` — a crawl's root, a folder's index page — has no such name
/// and was drawn as an empty entry (par:PAR-12); it is named for its last
/// segment, with the slash (`axum/`, `extract/`).
pub fn doc_label(name: &str, url: &str) -> String {
    if !name.is_empty() {
        return name.to_string();
    }
    match url.trim_end_matches('/').rsplit('/').next() {
        Some(seg) if !seg.is_empty() => format!("{seg}/"),
        _ => url.to_string(),
    }
}

/// The documents under their folders, relative to `prefix`, filtered by the
/// words typed: `("", top-level files)` first, then each folder in order.
pub fn doc_groups(
    docs: &[DocumentRow],
    prefix: &str,
    words: &[String],
) -> Vec<(String, Vec<(String, DocumentRow)>)> {
    let mut groups: Vec<(String, Vec<(String, DocumentRow)>)> = Vec::new();
    for d in docs {
        let rel = d.url.strip_prefix(prefix).unwrap_or(&d.url).to_string();
        let low = rel.to_lowercase();
        if !words.iter().all(|w| low.contains(w)) {
            continue;
        }
        let folder = rel
            .rsplit_once('/')
            .map(|(f, _)| f.to_string())
            .unwrap_or_default();
        match groups.iter_mut().find(|(f, _)| *f == folder) {
            Some((_, v)) => v.push((rel, d.clone())),
            None => groups.push((folder, vec![(rel, d.clone())])),
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    groups
}

#[component]
pub(super) fn CorpusBrowser() -> impl IntoView {
    let state = use_docs();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = state.browse.get().is_some();
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && state.browse.get_untracked().is_some() {
            state.browse.set(None);
        }
    });
    view! {
        <Modal open=open title="Corpus browser" size=ModalSize::Full fill=true>
            {move || {
                state
                    .browse
                    .get()
                    .map(|c| view! { <BrowserBody c=c/> }.into_any())
            }}
        </Modal>
    }
}

#[component]
fn BrowserBody(c: CorpusView) -> impl IntoView {
    let id = c.id;
    let docs = LocalResource::new(move || {
        crate::api::get::<DocumentsResponse>(format!("/api/docs/corpora/{id}/documents"))
    });
    let list = Memo::new(move |_| {
        docs.get()
            .and_then(|r| r.ok())
            .map(|d| d.documents)
            .unwrap_or_default()
    });
    let failed = Memo::new(move |_| docs.get().and_then(|r| r.err()).map(|e| e.to_string()));
    let prefix = Memo::new(move |_| list.with(|l| common_prefix(l.iter().map(|d| d.url.as_str()))));
    let query = RwSignal::new(String::new());
    let groups = Memo::new(move |_| {
        let words: Vec<String> = query
            .get()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        list.with(|l| prefix.with(|p| doc_groups(l, p, &words)))
    });
    let shown =
        Signal::derive(move || groups.with(|g| g.iter().map(|(_, v)| v.len()).sum::<usize>()));
    let total = Signal::derive(move || list.with(Vec::len));
    let open_doc = RwSignal::new(None::<DocumentRow>);
    // The first document opens with the browser: a chunk pane that starts
    // empty is a click nobody needed to make.
    Effect::new(move |_| {
        if open_doc.with_untracked(Option::is_some) {
            return;
        }
        let first = groups.with(|g| {
            g.first()
                .and_then(|(_, v)| v.first().map(|(_, d)| d.clone()))
        });
        if first.is_some() {
            open_doc.set(first);
        }
    });

    view! {
        <div class="cb">
            <div class="cb-head">
                <span class="mono-sm">{c.corpus_id.clone()}</span>
                <span class="dim">{grouped(c.chunk_count.max(0) as u64)} " chunks"</span>
                <span class="mono-sm dim cb-prefix" title=move || prefix.get()>{move || prefix.get()}</span>
                <span class="spacer"></span>
                <span class="dim mini-note">
                    "payloads are verbatim slices of the source — shown as-is, never re-rendered"
                </span>
            </div>
            <SplitPane
                side=Side::Left
                persist="docs.browser"
                label="Documents"
                badge=Signal::derive(move || crate::fmt::of(shown.get(), total.get()))
                width=(240, 30, 480)
                auto_collapse_below=420
                side_view=move || {
                    view! {
                        <div class="cb-list">
                            <FilterBar
                                query=query
                                placeholder="Filter documents"
                                shown=shown
                                total=total
                                noun="docs"
                            />
                            {move || match (docs.get().is_none(), failed.get()) {
                                (true, _) => view! { <div class="dim">"Loading…"</div> }.into_any(),
                                (_, Some(e)) => view! { <div class="notice err">{e}</div> }.into_any(),
                                _ if total.get() == 0 => {
                                    view! { <div class="empty">"No documents yet."</div> }.into_any()
                                }
                                _ => {
                                    view! {
                                        <For
                                            each=move || groups.get()
                                            key=|(f, v)| (f.clone(), v.len())
                                            let:g
                                        >
                                            <DocGroup folder=g.0 docs=g.1 open_doc=open_doc/>
                                        </For>
                                    }
                                        .into_any()
                                }
                            }}
                        </div>
                    }
                }
            >
                <div class="cb-chunks">
                    {move || match open_doc.get() {
                        None => {
                            view! { <div class="empty">"Pick a document to see its chunks."</div> }
                                .into_any()
                        }
                        Some(doc) => view! { <ChunkList doc=doc/> }.into_any(),
                    }}
                </div>
            </SplitPane>
        </div>
    }
}

/// One folder's documents under a heading with its count; the top level has
/// no heading.
#[component]
fn DocGroup(
    folder: String,
    docs: Vec<(String, DocumentRow)>,
    open_doc: RwSignal<Option<DocumentRow>>,
) -> impl IntoView {
    let n = docs.len();
    let cut = if folder.is_empty() {
        0
    } else {
        folder.len() + 1
    };
    view! {
        {(!folder.is_empty())
            .then(|| {
                view! {
                    <div class="cb-folder" title=folder.clone()>
                        <span class="cb-folder-name">{format!("{folder}/")}</span>
                        <span class="count">{n}</span>
                    </div>
                }
            })}
        {docs
            .into_iter()
            .map(|(rel, doc)| {
                let id = doc.id;
                let url = doc.url.clone();
                let name = doc_label(rel.get(cut..).unwrap_or(&rel), &url);
                let nested = cut != 0;
                let is_open = move || open_doc.with(|d| d.as_ref().map(|d| d.id) == Some(id));
                view! {
                    <button
                        class="cb-doc"
                        class:nested=nested
                        aria-current=move || is_open().to_string()
                        title=url
                        data-dock-pick
                        on:click=move |_| open_doc.set(Some(doc.clone()))
                    >
                        {name}
                    </button>
                }
            })
            .collect_view()}
    }
}

#[component]
fn ChunkList(doc: DocumentRow) -> impl IntoView {
    let id = doc.id;
    let chunks = LocalResource::new(move || {
        crate::api::get::<ChunksResponse>(format!("/api/docs/chunks?document_id={id}"))
    });
    view! {
        <div class="mini-head">
            <a href=doc.url.clone() target="_blank" rel="noreferrer">{doc.url.clone()}</a>
        </div>
        <div class="dim mono-sm cb-meta">
            "fetched " {doc.fetched_at.clone()} " · hash "
            <span title=doc.content_hash.clone()>
                {doc.content_hash.chars().take(12).collect::<String>()}
            </span>
            {move || {
                chunks
                    .get()
                    .and_then(|r| r.ok())
                    .map(|c| format!(" · {} chunks", c.chunks.len()))
            }}
        </div>
        {move || match chunks.get() {
            None => view! { <div class="dim">"Loading…"</div> }.into_any(),
            Some(Err(e)) => view! { <div class="notice err">{e.to_string()}</div> }.into_any(),
            Some(Ok(c)) if c.chunks.is_empty() => {
                view! { <div class="empty">"This document produced no chunks."</div> }.into_any()
            }
            Some(Ok(c)) => {
                view! {
                    <For each=move || c.chunks.clone() key=|c| c.id.clone() let:chunk>
                        <div class="card cb-chunk">
                            <div class="row">
                                <span class="type-badge">
                                    {format!("{}–{}", chunk.span_start, chunk.span_end)}
                                </span>
                                <span class="mono-sm">{chunk.heading_path.clone()}</span>
                                <span class="spacer"></span>
                                <crate::widgets::CopyBtn
                                    text=chunk.id.clone()
                                    title="Copy the chunk id (what a golden query expects)"
                                />
                            </div>
                            <DerivedLabels title=chunk.derived_title.clone() summary=chunk.derived_summary.clone()/>
                            <pre class="preset" style="white-space:pre-wrap; word-break:break-word; margin-top:8px">
                                {chunk.payload.clone()}
                            </pre>
                        </div>
                    </For>
                }
                    .into_any()
            }
        }}
    }
}

/// Derived title/summary are LLM output, so they are labelled as such and kept
/// visually apart from the verbatim payload below them (§8).
#[component]
fn DerivedLabels(title: String, summary: String) -> impl IntoView {
    let empty = title.trim().is_empty() && summary.trim().is_empty();
    view! {
        {(!empty)
            .then(|| {
                view! {
                    <div class="dim mini-note" style="margin-top:4px">
                        <span class="type-badge">"derived"</span>
                        " " {title} {(!summary.is_empty()).then(|| format!(" — {summary}"))}
                    </div>
                }
            })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared prefix is a directory, shown once; one document shares its
    /// own directory, and URLs that share nothing share nothing.
    #[test]
    fn the_shared_prefix_is_cut_back_to_a_directory() {
        let urls = [
            "https://raw.githubusercontent.com/leptos-rs/book/main/src/01_introduction.md",
            "https://raw.githubusercontent.com/leptos-rs/book/main/src/async/10_resources.md",
            "https://raw.githubusercontent.com/leptos-rs/book/main/src/SUMMARY.md",
        ];
        assert_eq!(
            common_prefix(urls),
            "https://raw.githubusercontent.com/leptos-rs/book/main/src/"
        );
        assert_eq!(
            common_prefix(["https://docs.rs/axum/latest/axum/index.html"]),
            "https://docs.rs/axum/latest/axum/"
        );
        assert_eq!(common_prefix(["a.md", "b.md"]), "");
        assert_eq!(common_prefix(std::iter::empty()), "");
    }

    /// Top-level files first, then each folder; the words filter the paths
    /// under the prefix, not the prefix itself.
    #[test]
    fn documents_group_by_folder_under_the_prefix() {
        let doc = |id: i64, url: &str| DocumentRow {
            id,
            url: url.to_string(),
            ..Default::default()
        };
        let p = "https://docs.example/src/";
        let docs = vec![
            doc(1, "https://docs.example/src/intro.md"),
            doc(2, "https://docs.example/src/router/16_routes.md"),
            doc(3, "https://docs.example/src/async/10_resources.md"),
            doc(4, "https://docs.example/src/zeta.md"),
        ];
        let g = doc_groups(&docs, p, &[]);
        let shape: Vec<(String, Vec<String>)> = g
            .iter()
            .map(|(f, v)| (f.clone(), v.iter().map(|(r, _)| r.clone()).collect()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (
                    "".to_string(),
                    vec!["intro.md".to_string(), "zeta.md".to_string()]
                ),
                (
                    "async".to_string(),
                    vec!["async/10_resources.md".to_string()]
                ),
                (
                    "router".to_string(),
                    vec!["router/16_routes.md".to_string()]
                ),
            ]
        );
        let only = doc_groups(&docs, p, &["rout".to_string()]);
        assert_eq!(only.len(), 1);
        assert!(doc_groups(&docs, p, &["docs.example".to_string()]).is_empty());

        // A crawl's root and a folder's index page end in "/": each keeps a
        // place in its group and gets a name, never an empty entry.
        let docs = vec![
            doc(1, "https://docs.rs/axum/0.8.0/axum/"),
            doc(2, "https://docs.rs/axum/0.8.0/axum/extract/"),
            doc(
                3,
                "https://docs.rs/axum/0.8.0/axum/extract/struct.Path.html",
            ),
        ];
        let p = common_prefix(docs.iter().map(|d| d.url.as_str()));
        assert_eq!(p, "https://docs.rs/axum/0.8.0/axum/");
        let g = doc_groups(&docs, &p, &[]);
        let names: Vec<(String, Vec<String>)> = g
            .iter()
            .map(|(f, v)| {
                let cut = if f.is_empty() { 0 } else { f.len() + 1 };
                let names = v
                    .iter()
                    .map(|(r, d)| doc_label(r.get(cut..).unwrap_or(r), &d.url))
                    .collect();
                (f.clone(), names)
            })
            .collect();
        assert_eq!(
            names,
            vec![
                ("".to_string(), vec!["axum/".to_string()]),
                (
                    "extract".to_string(),
                    vec!["extract/".to_string(), "struct.Path.html".to_string()]
                ),
            ]
        );
    }
}
