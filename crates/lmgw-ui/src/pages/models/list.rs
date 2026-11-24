//! The list pane (modern-sample.html): a header with the title, the class
//! and state filter under it and the add actions, a search entry, and the
//! rows grouped by class under sticky headings. Each row is a two-line
//! [`ListRow`]: name, state dot and word, and a line of facts.
//! The header, search and keys are the shared list pane's
//! ([`crate::widgets::list_pane`]).

use leptos::html;
use leptos::prelude::*;
use lmgw_api_types::RuntimeStatus;

use super::route::Sel;
use super::{
    blank_audio_model, blank_aux_model, blank_image_model, is_running, runtime_of, CatalogLine,
    Class, PageCtx, Row,
};
use std::collections::HashMap;

use crate::fmt::{fmt_bytes, grouped};
use crate::widgets::list_pane::{
    GroupHead, ListBox, ListEmpty, ListHead, ListNav, ListNotice, ListRefs, ListSearch,
};
use crate::widgets::{filter_words, Dot, Ico, Icon, ListRow, MenuItem, Popover, RowMenu};

/// What a list line is.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum Entry {
    /// A group's sticky heading and how many rows it shows.
    Head(&'static str, usize),
    /// The line of a class its own filter shows with nothing in it: what
    /// fills it.
    Empty(Class),
    Row(Box<Row>),
    Catalog(CatalogLine),
}

impl Entry {
    pub(super) fn sel(&self) -> Option<Sel> {
        match self {
            Self::Row(r) => Some(Sel::Row(r.class, r.id)),
            Self::Catalog(l) => Some(Sel::Catalog(l.id)),
            _ => None,
        }
    }
}

impl Class {
    /// The list's group heading.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Chat => "Local chat",
            Self::Aux => "Embeddings, rerank and decisions",
            Self::Audio => "Audio",
            Self::Image => "Image",
            Self::Alias => "Cloud aliases",
            Self::Candidate => "Candidate aliases",
        }
    }

    pub(super) fn icon(self) -> Ico {
        match self {
            Self::Chat => Ico::Chat,
            Self::Aux => Ico::Split,
            Self::Audio => Ico::Wave,
            Self::Image => Ico::Image,
            Self::Alias => Ico::Cloud,
            Self::Candidate => Ico::Shuffle,
        }
    }
}

/// The filter's choices: (facet id, label). The ids are the `?show=` values
/// the page has always had.
fn facets() -> Vec<(&'static str, &'static str)> {
    let mut v = vec![
        ("", "All classes"),
        ("running", "Running"),
        ("disabled", "Disabled"),
    ];
    v.extend(Class::ALL.iter().map(|c| (c.id(), c.label())));
    v
}

/// The quantization a GGUF's file name states ("Q4_K_M", "UD-Q4_K_XL",
/// "IQ2_XXS", "BF16"), if it states one.
pub(super) fn quant_of(file: &str) -> Option<String> {
    let stem = file
        .rsplit('/')
        .next()
        .unwrap_or(file)
        .trim_end_matches(".gguf");
    let parts: Vec<&str> = stem.split(['-', '.']).collect();
    let is_quant = |p: &str| {
        let u = p.to_ascii_uppercase();
        let digit_after = |pre: &str| {
            u.strip_prefix(pre)
                .and_then(|r| r.chars().next())
                .is_some_and(|c| c.is_ascii_digit())
        };
        digit_after("Q")
            || digit_after("IQ")
            || digit_after("TQ")
            || matches!(u.as_str(), "F16" | "BF16" | "F32" | "MXFP4")
    };
    let i = parts.iter().rposition(|p| is_quant(p))?;
    let q = parts[i].to_string();
    match i.checked_sub(1).map(|j| parts[j]) {
        Some(ud) if ud.eq_ignore_ascii_case("UD") => Some(format!("UD-{q}")),
        _ => Some(q),
    }
}

/// The bytes of every GGUF in the models dir by its path; a model split in
/// shards (`…-00001-of-00003.gguf`) counts all of them under its first.
pub(super) fn gguf_sizes(files: &[(String, u64)]) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for (path, bytes) in files {
        *out.entry(first_shard(path)).or_default() += bytes;
    }
    out
}

/// A shard's path as its model's first shard's (`-00002-of-00003.gguf` →
/// `-00001-of-00003.gguf`); any other path as it is.
fn first_shard(path: &str) -> String {
    let Some(stem) = path.strip_suffix(".gguf") else {
        return path.to_string();
    };
    let mut parts = stem.rsplitn(4, '-');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(of_n), Some("of"), Some(n), Some(head))
            if n.len() == 5
                && of_n.len() == 5
                && n.chars().chain(of_n.chars()).all(|c| c.is_ascii_digit()) =>
        {
            format!("{head}-00001-of-{of_n}.gguf")
        }
        _ => path.to_string(),
    }
}

/// The row's second line: what it is, in a few facts. `bytes` is its GGUF's
/// size on disk, when the models dir was read.
pub(super) fn meta_of(r: &Row, bytes: Option<u64>) -> String {
    let mut parts: Vec<String> = Vec::new();
    match r.class {
        Class::Chat => {
            parts.push(quant_of(&r.file).unwrap_or_else(|| r.file.clone()));
            if let Some(b) = bytes {
                parts.push(fmt_bytes(b));
            }
            if let Some(c) = r.ctx {
                parts.push(format!("{} context", grouped(c.max(0) as u64)));
            }
            if !r.note.is_empty() {
                parts.push(r.note.clone());
            }
        }
        Class::Aux => {
            parts.push(match r.kind.as_str() {
                "embed" => "Embeddings".to_string(),
                "rerank" => "Rerank".to_string(),
                "decision" => "Decisions".to_string(),
                k => crate::labels::category_label(k),
            });
            parts.push(quant_of(&r.file).unwrap_or_else(|| r.file.clone()));
        }
        Class::Audio | Class::Image => {
            parts.push(r.kind.clone());
            if !r.note.is_empty() {
                parts.push(r.note.clone());
            }
            parts.push(r.file.clone());
        }
        Class::Alias | Class::Candidate => parts.push(r.file.clone()),
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

/// A row's state as the list says it: the dot and its word. `rt` is its
/// container's entry in the runtime frame, `downloading` whether its start
/// waits for the download for the graphics card.
pub(super) fn state_of(r: &Row, rt: Option<&RuntimeStatus>, downloading: bool) -> (Dot, String) {
    if !r.enabled {
        return (Dot::Off, "Disabled".into());
    }
    match r.class {
        Class::Alias => return (Dot::Remote, "Ready".into()),
        Class::Candidate if r.problems > 0 => {
            let n = r.problems;
            return (
                Dot::Error,
                format!("{n} problem{}", if n == 1 { "" } else { "s" }),
            );
        }
        Class::Candidate => return (Dot::Remote, "Ready".into()),
        _ => {}
    }
    match rt.map(|r| r.state.as_str()) {
        Some("ready") => (Dot::Running, "Running".into()),
        Some("starting") => (Dot::Busy, "Starting".into()),
        Some("stopping") => (Dot::Busy, "Stopping".into()),
        Some(other) => (Dot::Busy, other.to_string()),
        None if downloading => (Dot::Busy, "Downloading".into()),
        None => (Dot::Stopped, "Stopped".into()),
    }
}

/// The list's lines under the filter and the search, and how many
/// configured rows they show.
fn entries_of(
    rows: &[Row],
    lines: &[CatalogLine],
    words: &[String],
    facet: &str,
    rt: &[RuntimeStatus],
) -> (Vec<Entry>, usize) {
    let keep = |r: &Row| {
        r.matches(words)
            && match facet {
                "" => true,
                "disabled" => !r.enabled,
                "running" => is_running(rt, r),
                id => r.class.id() == id,
            }
    };
    let mut out = Vec::new();
    let mut shown = 0;
    for c in Class::ALL {
        let mine: Vec<&Row> = rows.iter().filter(|r| r.class == c && keep(r)).collect();
        if mine.is_empty() {
            // A class's own filter keeps its heading: the line under it says
            // how the first one is made.
            if facet == c.id() {
                out.push(Entry::Head(c.label(), 0));
                out.push(Entry::Empty(c));
            }
            continue;
        }
        shown += mine.len();
        out.push(Entry::Head(c.label(), mine.len()));
        out.extend(mine.into_iter().cloned().map(|r| Entry::Row(Box::new(r))));
    }
    // The catalogs are no configured models: a class or state filter leaves
    // them out, the search matches the upstream's name.
    if facet.is_empty() {
        let hits: Vec<&CatalogLine> = lines
            .iter()
            .filter(|l| {
                let hay = format!("{} {}", l.name, l.prefix).to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect();
        if !hits.is_empty() {
            out.push(Entry::Head("Upstream catalogs", hits.len()));
            out.extend(hits.into_iter().cloned().map(Entry::Catalog));
        }
    }
    (out, shown)
}

#[component]
pub(super) fn ListPane(
    page: PageCtx,
    sel: Memo<Option<Sel>>,
    narrow: Memo<bool>,
    refs: ListRefs<Sel>,
    /// Open a row (a click, Enter, or the selection moving in a wide window).
    open_row: Callback<(Sel, bool)>,
) -> impl IntoView {
    let live = crate::live::use_live();
    let catalog = crate::catalog::use_model_catalog();
    let data = page.data;
    let query = crate::url_state::use_query_signal("q");
    let facet = crate::url_state::use_query_signal("show");

    let rows = Memo::new(move |_| {
        data.full
            .with(|f| f.as_ref().map(super::rows_of).unwrap_or_default())
    });
    // The GGUFs' sizes on disk, for the chat rows' line: read again with
    // the list.
    let files = LocalResource::new(move || {
        page.reload.0.track();
        crate::api::get::<lmgw_api_types::GgufFiles>("/api/gguf-files")
    });
    let sizes = Memo::new(move |_| {
        files
            .get()
            .and_then(Result::ok)
            .map(|g| {
                let files: Vec<(String, u64)> = g
                    .files
                    .iter()
                    .map(|f| (f.path.clone(), f.size_bytes))
                    .collect();
                gguf_sizes(&files)
            })
            .unwrap_or_default()
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
    let shown = Memo::new(move |_| {
        let words = filter_words(&query.get());
        let f = facet.get();
        // Only the Running filter reads the runtime frame, so the list is not
        // derived again on every frame the bus sends.
        let rt = if f == "running" {
            live.runtime.get().unwrap_or_default()
        } else {
            Vec::new()
        };
        rows.with(|rs| lines.with(|ls| entries_of(rs, ls, &words, &f, &rt)))
    });
    let order = Memo::new(move |_| {
        shown.with(|(es, _)| es.iter().filter_map(Entry::sel).collect::<Vec<_>>())
    });

    // The filter's counts: every configured row, not only the shown ones.
    let counts = Memo::new(move |_| {
        let rt = live.runtime.get().unwrap_or_default();
        rows.with(|rs| {
            facets()
                .into_iter()
                .map(|(id, _)| match id {
                    "" => rs.len(),
                    "running" => rs.iter().filter(|r| is_running(&rt, r)).count(),
                    "disabled" => rs.iter().filter(|r| !r.enabled).count(),
                    id => rs.iter().filter(|r| r.class.id() == id).count(),
                })
                .collect::<Vec<_>>()
        })
    });
    let filter_label = move || {
        let f = facet.get();
        let label = facets()
            .into_iter()
            .find(|(id, _)| *id == f)
            .map_or("All classes", |(_, l)| l);
        match data.full.with(Option::is_some) {
            true => format!("{label} · {}", shown.with(|(_, n)| *n)),
            false if data.error.with(Option::is_some) => "not loaded".to_string(),
            false => "Loading…".to_string(),
        }
    };

    let nav = ListNav::new(refs, order, sel, narrow, open_row, |s: &Sel| s.path());

    let fopen = RwSignal::new(false);
    let fbtn: NodeRef<html::Button> = NodeRef::new();
    let add_items = add_menu(page);

    let render = move |e: Entry| -> AnyView {
        match e {
            Entry::Head(label, n) => view! { <GroupHead label=label n=n/> }.into_any(),
            Entry::Empty(c) => {
                view! { <div class="mlist-empty-class">{c.empty()}</div> }.into_any()
            }
            Entry::Row(r) => {
                let s = Sel::Row(r.class, r.id);
                let r = StoredValue::new(*r);
                let (class, mid) = r.with_value(|r| (r.class, r.model_id.clone()));
                let dl = crate::gpu_download::live_for(
                    class.runtime().unwrap_or_default().to_string(),
                    mid,
                );
                let st = Memo::new(move |_| {
                    let downloading = dl.with(Option::is_some);
                    r.with_value(|row| {
                        live.runtime.with(|rt| {
                            let me = rt
                                .as_deref()
                                .and_then(|rt| runtime_of(rt, row.class, &row.model_id));
                            state_of(row, me, downloading)
                        })
                    })
                });
                view! {
                    <ListRow
                        key=s.path()
                        name=r.with_value(|r| r.name.clone())
                        meta=move || {
                            r.with_value(|r| {
                                let bytes = (r.class == Class::Chat)
                                    .then(|| sizes.with(|m| m.get(&r.copy).copied()))
                                    .flatten();
                                meta_of(r, bytes)
                            })
                        }
                        dot=Signal::derive(move || st.get().0)
                        state=move || st.get().1
                        selected=Signal::derive(move || nav.lit(&s))
                        off=Signal::stored(!r.with_value(|r| r.enabled))
                        on_click=Callback::new(move |()| nav.click(s))
                    />
                }
                .into_any()
            }
            Entry::Catalog(l) => {
                let s = Sel::Catalog(l.id);
                let name = l.name.clone();
                let total = Signal::derive(move || {
                    catalog
                        .entries
                        .with(|es| es.iter().filter(|e| !e.local && e.source == name).count())
                        + l.hidden
                });
                let prefix = l.prefix.trim_matches('/').to_string();
                view! {
                    <ListRow
                        key=s.path()
                        name=l.name.clone()
                        meta=format!("requests as {prefix}/<id>")
                        dot=Signal::stored(Dot::Remote)
                        state=move || format!("{} models", grouped(total.get() as u64))
                        selected=Signal::derive(move || nav.lit(&s))
                        on_click=Callback::new(move |()| nav.click(s))
                    />
                }
                .into_any()
            }
        }
    };

    let filter = ViewFn::from(move || {
        view! {
            <button
                type="button"
                class="filter-btn"
                node_ref=fbtn
                aria-haspopup="menu"
                aria-expanded=move || fopen.get().to_string()
                title="Show one class, or the running or disabled models"
                on:click=move |_| fopen.update(|o| *o = !*o)
            >
                <span>{filter_label}</span>
                <Icon ico=Ico::Down/>
            </button>
        }
    });
    let actions = ViewFn::from(move || {
        view! {
            <a class="icon-btn" href="/downloads" title="Downloads">
                <Icon ico=Ico::Download/>
            </a>
            <RowMenu items=add_items icon=Ico::Plus title="Add a model"/>
        }
    });

    view! {
        <ListHead scrolled=nav.scrolled narrow=narrow title="Models" under=filter actions=actions/>
        <Popover open=fopen anchor=fbtn class="menu-pop">
            <div class="pop-list menu-list" role="menu" tabindex="-1" data-autofocus>
                {facets()
                    .into_iter()
                    .enumerate()
                    .map(|(i, (id, label))| {
                        view! {
                            {(i == 3).then(|| view! { <hr class="menu-sep"/> })}
                            <button
                                type="button"
                                class="menu-item"
                                class:checked=move || facet.get() == id
                                role="menuitemradio"
                                aria-checked=move || (facet.get() == id).to_string()
                                on:click=move |_| {
                                    facet.set(id.to_string());
                                    fopen.set(false);
                                }
                            >
                                <span class="menu-check"></span>
                                <span class="menu-label">{label}</span>
                                <span class="menu-count">
                                    {move || counts.with(|c| c.get(i).copied().unwrap_or(0))}
                                </span>
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
        </Popover>
        <ListSearch nav=nav query=query placeholder="Search models"/>
        <ListNotice
            error=data.error
            stale=Signal::derive(move || data.full.with(Option::is_some))
            failed="Loading models failed: "
            retry=Callback::new(move |()| data.load())
        />
        <ListBox nav=nav label="Models">
            <ListEmpty
                loaded=Signal::derive(move || data.full.with(Option::is_some))
                error=data.error
                empty=Signal::derive(move || shown.with(|(es, _)| es.is_empty()))
                query=query
                none="Nothing here yet: add a model with +"
                no_match="No models match"
            />
            <For each=move || shown.with(|(es, _)| es.clone()) key=|e| e.clone() let:e>
                {render(e)}
            </For>
        </ListBox>
    }
}

/// The ways to add a model, behind the header's +: the finder and the wizard
/// first, then each class's own.
fn add_menu(page: PageCtx) -> Signal<Vec<MenuItem>> {
    let editors = page.editors;
    let catalog_open = expect_context::<super::CatalogOpen>();
    let recipes_open = expect_context::<super::RecipesOpen>();
    Signal::derive(move || {
        vec![
            MenuItem::new("Find a model…", move || {
                page.go("/models/find", false);
            }).title(
                "Find a model by the name someone told you: lmgw picks the file that fits your graphics card",
            ),
            MenuItem::new("Add from Hugging Face…", move || page.wizard_open.set(true))
                .title("Search a repository, pick a file, download it; the row is made when it lands"),
            MenuItem::new("Local chat model from a file…", move || {
                page.go(&Sel::NewChat.path(), false);
            })
            .separated()
            .title("Wire up a GGUF that is already in the models dir"),
            MenuItem::new("Aux model from a file…", move || {
                editors.aux.set(Some(blank_aux_model()))
            })
            .title(
                "Wire up an embedding, rerank or decision GGUF that is already in the aux models dir; Fill from plan reads its header",
            ),
            MenuItem::new("Audio model…", move || {
                editors.audio.set(Some(blank_audio_model()))
            }),
            MenuItem::new("Image model…", move || {
                editors.image_derived.set(None);
                editors.image.set(Some(blank_image_model()));
            }),
            MenuItem::new("Audio model from the catalog…", move || catalog_open.0.set(true))
                .separated()
                .title("Browse audio.cpp's model families and install a package"),
            MenuItem::new("Image model from a recipe…", move || recipes_open.0.set(true)).title(
                "Shipped stable-diffusion.cpp pipelines — download every component, then fill the editor",
            ),
            MenuItem::new("Cloud alias…", move || {
                editors.alias.set(Some(lmgw_api_types::AliasView::default()))
            })
            .separated()
            .title("Map a name clients request to a model of an upstream"),
            MenuItem::new("Candidate alias…", move || {
                editors
                    .candidate
                    .set(Some(crate::pages::candidate_alias_editor::blank_candidate_alias()))
            })
            .title(
                "A client-facing name backed by a primary local model plus alternates, used only when already loaded",
            ),
            MenuItem::new("Upstream catalogs", move || {
                page.go("/models/catalog", false);
            })
                .separated()
                .title("Every model of an expose-all upstream, resolved live"),
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_model_counts_every_shard() {
        let files = vec![
            ("a/m-00001-of-00002.gguf".to_string(), 10),
            ("a/m-00002-of-00002.gguf".to_string(), 7),
            ("b/one.gguf".to_string(), 3),
            ("c/x-of-y.gguf".to_string(), 1),
        ];
        let m = gguf_sizes(&files);
        assert_eq!(m.get("a/m-00001-of-00002.gguf"), Some(&17));
        assert_eq!(m.get("b/one.gguf"), Some(&3));
        assert_eq!(m.get("c/x-of-y.gguf"), Some(&1));
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn a_file_name_states_its_quant() {
        assert_eq!(
            quant_of("Qwen3.8-Flash-Next-Q4_K_M.gguf").as_deref(),
            Some("Q4_K_M")
        );
        assert_eq!(
            quant_of("models/gemma-4-27b-it-UD-Q4_K_XL.gguf").as_deref(),
            Some("UD-Q4_K_XL")
        );
        assert_eq!(
            quant_of("DeepSeek-V4-Flash-IQ2_XXS-00001-of-00003.gguf").as_deref(),
            Some("IQ2_XXS")
        );
        assert_eq!(quant_of("model.BF16.gguf").as_deref(), Some("BF16"));
        assert_eq!(
            quant_of("phi-5-mini-instruct-Q8_0.gguf").as_deref(),
            Some("Q8_0")
        );
        assert_eq!(quant_of("no-quant-here.gguf"), None);
        // A model name's own "Qwen" is no quant.
        assert_eq!(quant_of("Qwen3-Embedding.gguf"), None);
    }
}
