//! Knowledge — the owner's own documents in named collections (chat-complete
//! design §9.5).
//!
//! The **list** at `/knowledge` (name, files, chunks, embedding model, status,
//! the live job) and one base's **detail** at `/knowledge/:id/:tab?` with the
//! Files, Search and Settings tabs. The backend is `/api/knowledge/*`; live
//! progress is the ordinary jobs feed (`kb_ingest` / `kb_reembed`, key
//! `kb:<id>`), consumed the way the Docs page consumes its own: the durable
//! rows are re-read when a job starts, ends or finishes a file, and every
//! tick in between arrives on the bus — nothing here polls.

use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::use_params_map;
use lmgw_api_types::JobRow;
use serde_json::{json, Value};

use crate::fmt::{grouped, human_bytes};
use crate::scope::Scope;
use crate::widgets::{use_toasts, NavTab, PageFrame, PageMode, SubNav, Tone};

/// The two job kinds that report against a base; both use the key `kb:<id>`.
pub const KB_JOB_KINDS: [&str; 2] = ["kb_ingest", "kb_reembed"];

// ---------------------------------------------------------------------------
// What the API sends
// ---------------------------------------------------------------------------

pub use lmgw_api_types::knowledge::{
    KnowledgeBase as Kb, KnowledgeBaseDetail as DetailResponse, KnowledgeBaseList as BasesResponse,
    KnowledgeFile as KbFile,
};

// ---------------------------------------------------------------------------
// Live job
// ---------------------------------------------------------------------------

/// The job running against one base right now, of either kind.
pub fn kb_job(id: i64) -> Option<JobRow> {
    let key = format!("kb:{id}");
    crate::live::use_live()
        .jobs
        .get()
        .unwrap_or_default()
        .into_iter()
        .find(|j| KB_JOB_KINDS.contains(&j.kind.as_str()) && j.key.as_deref() == Some(&key))
}

/// Bump `reload` when a knowledge job starts, ends, or finishes a file — the
/// same trigger Docs uses, with `done` added so the file table follows an
/// ingest one file at a time. Made beside the signals it writes.
fn reload_on_jobs(reload: RwSignal<u32>) {
    let live = crate::live::use_live();
    let marks = Memo::new(move |_| {
        let mut m: Vec<(i64, u64, String)> = live
            .jobs
            .get()
            .unwrap_or_default()
            .iter()
            .filter(|j| KB_JOB_KINDS.contains(&j.kind.as_str()))
            .map(|j| (j.id, j.done, j.stage.clone()))
            .collect();
        m.sort();
        m
    });
    Effect::new(move |prev: Option<Vec<(i64, u64, String)>>| {
        let now = marks.get();
        if prev.is_some_and(|p| p != now) {
            reload.update(|n| *n += 1);
        }
        now
    });
}

fn job_unit(kind: &str) -> &'static str {
    if kind == "kb_reembed" {
        "chunks"
    } else {
        "files"
    }
}

fn job_title(kind: &str) -> &'static str {
    if kind == "kb_reembed" {
        "re-embedding"
    } else {
        "ingesting"
    }
}

/// The live job's line: what it is doing, how far, and Cancel.
#[component]
pub fn KbJobLine(id: i64, #[prop(optional)] compact: bool) -> impl IntoView {
    let toasts = use_toasts();
    let job = Memo::new(move |_| kb_job(id));
    let cancel = move |_| {
        let Some(j) = job.get_untracked() else { return };
        let _ = j;
        leptos::task::spawn_local(async move {
            match crate::api::post::<Value, _>(
                format!("/api/knowledge/bases/{id}/cancel"),
                &json!({}),
            )
            .await
            {
                Ok(_) => toasts.ok("cancel requested"),
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        {move || {
            job.get()
                .map(|j| {
                    let unit = job_unit(&j.kind);
                    let counts = match j.total {
                        Some(t) => format!("{} / {t} {unit}", j.done),
                        None => format!("{} {unit}", j.done),
                    };
                    let file = j
                        .detail
                        .get("file")
                        .and_then(Value::as_str)
                        .filter(|f| !f.is_empty())
                        .map(|f| {
                            let stage = j
                                .detail
                                .get("file_stage")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            format!("{f} · {stage}")
                        });
                    view! {
                        <div class="dl-line">
                            <span class="mono-sm">
                                {job_title(&j.kind)} " · " {j.stage.clone()}
                                {(!compact)
                                    .then(|| file.clone())
                                    .flatten()
                                    .map(|f| view! { <span class="dim">" · " {f}</span> })}
                            </span>
                            <span class="row" style="flex-wrap:nowrap; justify-self:end">
                                <span class="dim mono-sm">{counts}</span>
                                {(!compact)
                                    .then(|| {
                                        view! {
                                            <button class="btn ghost sm" on:click=cancel>
                                                "Cancel"
                                            </button>
                                        }
                                    })}
                            </span>
                            <div class="progress">
                                <i style=format!("width:{}%", j.percent.unwrap_or(3))></i>
                            </div>
                        </div>
                    }
                })
        }}
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// What the base needs, in one chip: the live job when there is one, else the
/// most pressing thing the counts say.
#[component]
pub fn KbStatus(kb: Kb) -> impl IntoView {
    let id = kb.id;
    let live = Memo::new(move |_| kb_job(id));
    let c = kb.counts.clone();
    let reembed = kb.status == "re_embed_required" || c.embedded < c.chunks;
    let last_failed = kb
        .job
        .as_ref()
        .is_some_and(|j| j.status == "failed" && j.error.is_some());
    let last_error = kb
        .job
        .as_ref()
        .and_then(|j| j.error.clone())
        .unwrap_or_default();
    view! {
        {move || match live.get() {
            Some(j) => {
                let n = match j.total {
                    Some(t) => format!("{} {}/{t}", job_title(&j.kind), j.done),
                    None => job_title(&j.kind).to_string(),
                };
                view! {
                    <span class="chip live">
                        <span class="dot"></span>
                        {n}
                    </span>
                }
                    .into_any()
            }
            None if last_failed && (c.pending > 0 || reembed) => {
                view! {
                    <span class="chip warn" title=last_error.clone()>
                        <span class="dot"></span>
                        "stopped — Resume"
                    </span>
                }
                    .into_any()
            }
            None if reembed => {
                view! {
                    <span class="chip err" title="some chunks have no vector for the pinned model">
                        <span class="dot"></span>
                        "re-embed required"
                    </span>
                }
                    .into_any()
            }
            None if c.failed > 0 => {
                view! {
                    <span class="chip err">
                        <span class="dot"></span>
                        {format!("{} failed", c.failed)}
                    </span>
                }
                    .into_any()
            }
            None if c.pending > 0 => {
                view! {
                    <span class="chip warn">
                        <span class="dot"></span>
                        {format!("{} waiting", c.pending)}
                    </span>
                }
                    .into_any()
            }
            None if c.files == 0 => {
                view! {
                    <span class="chip off">
                        <span class="dot"></span>
                        "empty"
                    </span>
                }
                    .into_any()
            }
            None => {
                view! {
                    <span class="chip ok">
                        <span class="dot"></span>
                        "ready"
                    </span>
                }
                    .into_any()
            }
        }}
    }
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[component]
pub fn Knowledge() -> impl IntoView {
    let reload = RwSignal::new(0u32);
    let bases = RwSignal::new(Vec::<Kb>::new());
    let loaded = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let new_open = RwSignal::new(false);

    let res = LocalResource::new(move || {
        reload.get();
        crate::api::get::<BasesResponse>("/api/knowledge/bases")
    });
    Effect::new(move |_| match res.get() {
        Some(Ok(v)) => {
            bases.set(v.bases);
            error.set(None);
            loaded.set(true);
        }
        Some(Err(e)) => error.set(Some(e.to_string())),
        None => {}
    });
    reload_on_jobs(reload);

    let total = Signal::derive(move || bases.with(Vec::len));
    let files = move || bases.with(|b| b.iter().map(|k| k.counts.files).sum::<i64>());
    let chunks = move || bases.with(|b| b.iter().map(|k| k.counts.chunks).sum::<i64>());

    view! {
        <PageFrame
            title="Knowledge"
            sub="Your documents, in collections — searched from Chat and served on /mcp"
            mode=PageMode::Fill
            class="knowledge"
            actions=move || {
                view! {
                    <button class="btn primary" on:click=move |_| new_open.set(true)>
                        "New knowledge base"
                    </button>
                }
            }
        >
            <div class="docs-meta dim">
                <span>
                    {move || grouped(total.get() as u64)} " bases · " {move || grouped(files() as u64)}
                    " files · " {move || grouped(chunks() as u64)} " chunks"
                </span>
            </div>
            {move || {
                error
                    .get()
                    .map(|e| {
                        view! {
                            <div class="notice err">
                                "Failed to load knowledge bases: " {e} " "
                                <button
                                    class="link-btn"
                                    on:click=move |_| reload.update(|n| *n += 1)
                                >
                                    "Retry"
                                </button>
                            </div>
                        }
                    })
            }}
            <div class="fill-pane hug card pad0">
                <table class="data kb-table">
                    <thead>
                        <tr>
                            <th>"Knowledge base"</th>
                            <th>"Status"</th>
                            <th class="num-h">"Files"</th>
                            <th class="num-h">"Chunks"</th>
                            <th>"Embedding"</th>
                        </tr>
                    </thead>
                    <tbody>
                        <For each=move || bases.get() key=|k| (k.id, k.updated_at.clone(), k.counts.clone(), k.status.clone()) let:k>
                            <KbRow kb=k/>
                        </For>
                    </tbody>
                </table>
                <Show when=move || !loaded.get() && error.with(Option::is_none)>
                    <div class="empty">"Loading…"</div>
                </Show>
                <Show when=move || loaded.get() && total.get() == 0>
                    <div class="empty">
                        "No knowledge bases yet — make one, then drop your files into it."
                    </div>
                </Show>
            </div>
            <super::knowledge_new::NewKbModal open=new_open on_created=Callback::new(move |_| reload.update(|n| *n += 1))/>
        </PageFrame>
    }
}

#[component]
fn KbRow(kb: Kb) -> impl IntoView {
    let href = format!("/knowledge/{}", kb.id);
    let id = kb.id;
    let unresolved = !kb.embed_resolvable;
    let name = kb.name.clone();
    let description = kb.description.clone();
    let identity = kb.embed_identity.clone();
    let alias = kb.embed_alias.clone();
    view! {
        <tr>
            <td>
                <A href=href>{name}</A>
                {(!description.is_empty())
                    .then(|| view! { <div class="dim mini-note">{description}</div> })}
            </td>
            <td>
                <KbStatus kb=kb.clone()/>
                <KbJobLine id=id compact=true/>
            </td>
            <td class="num">
                {grouped(kb.counts.files as u64)}
                <span class="dim mono-sm">" · " {human_bytes(kb.counts.bytes.max(0) as u64)}</span>
            </td>
            <td class="num">{grouped(kb.counts.chunks as u64)}</td>
            <td>
                <span class="mono-sm" title=identity>{alias}</span>
                {unresolved
                    .then(|| {
                        view! {
                            <span
                                class="chip err"
                                title="no model on this gateway resolves to this pin any more — searches fall back to keywords"
                            >
                                <span class="dot"></span>
                                "model gone"
                            </span>
                        }
                    })}
            </td>
        </tr>
    }
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

/// What a base's tabs share: the base, its files, and the reload trigger.
#[derive(Clone, Copy)]
pub struct KbCtx {
    pub id: i64,
    pub kb: RwSignal<Option<Kb>>,
    pub files: RwSignal<Vec<KbFile>>,
    pub reload: RwSignal<u32>,
    pub scope: Scope,
}

impl KbCtx {
    pub fn bump(&self) {
        self.reload.update(|n| *n += 1);
    }
}

pub fn use_kb() -> KbCtx {
    expect_context::<KbCtx>()
}

pub const KB_TABS: &[&str] = &["files", "search", "settings"];

fn kb_href(id: i64, tab: &str) -> String {
    match tab {
        "search" | "settings" => format!("/knowledge/{id}/{tab}"),
        _ => format!("/knowledge/{id}"),
    }
}

/// `/knowledge/:id/:tab?`
#[component]
pub fn KnowledgeDetail() -> impl IntoView {
    let params = use_params_map();
    // The id in the path is the page's identity: a different one is a new
    // page, so the whole body is rebuilt under it.
    move || {
        let id = params.with(|p| p.get_str("id").and_then(|s| s.parse::<i64>().ok()));
        match id {
            Some(id) => view! { <DetailBody id=id/> }.into_any(),
            None => view! {
                <PageFrame title="Knowledge" sub="no such knowledge base">
                    <div class="card">
                        "That is not a knowledge base id. " <A href="/knowledge">"All knowledge bases"</A>
                    </div>
                </PageFrame>
            }
            .into_any(),
        }
    }
}

#[component]
fn DetailBody(id: i64) -> impl IntoView {
    let ctx = KbCtx {
        id,
        kb: RwSignal::new(None),
        files: RwSignal::new(Vec::new()),
        reload: RwSignal::new(0),
        scope: Scope::new(),
    };
    provide_context(ctx);
    let error = RwSignal::new(None::<String>);
    let tab = crate::url_state::use_view("tab", KB_TABS, "files");

    let res = LocalResource::new(move || {
        ctx.reload.get();
        crate::api::get::<DetailResponse>(format!("/api/knowledge/bases/{id}"))
    });
    Effect::new(move |_| match res.get() {
        Some(Ok(v)) => {
            ctx.kb.set(Some(v.base));
            ctx.files.set(v.files);
            error.set(None);
        }
        Some(Err(e)) => error.set(Some(e.to_string())),
        None => {}
    });
    reload_on_jobs(ctx.reload);

    let tabs = Signal::derive(move || {
        let files = ctx.kb.with(|k| k.as_ref().map(|k| k.counts.files));
        let mut f = NavTab::new("Files", kb_href(id, "files")).exact();
        if let Some(n) = files {
            f = f.count(n);
        }
        let failed = ctx
            .kb
            .with(|k| k.as_ref().map(|k| k.counts.failed).unwrap_or(0));
        if failed > 0 {
            f = f.tone(Tone::Bad);
        }
        vec![
            f,
            NavTab::new("Search", kb_href(id, "search")),
            NavTab::new("Settings", kb_href(id, "settings")),
        ]
    });
    let title = Signal::derive(move || {
        ctx.kb
            .with(|k| k.as_ref().map(|k| k.name.clone()))
            .unwrap_or_else(|| "Knowledge base".into())
    });
    let sub = Signal::derive(move || {
        ctx.kb
            .with(|k| k.as_ref().map(|k| k.description.clone()))
            .unwrap_or_default()
    });

    view! {
        <PageFrame
            title=title
            sub=sub
            class="knowledge"
            head_extra=move || view! { <SubNav tabs=tabs/> }
            actions=move || {
                view! { <A href="/knowledge" attr:class="btn ghost">"All knowledge bases"</A> }
            }
        >
            {move || {
                error
                    .get()
                    .map(|e| {
                        view! {
                            <div class="notice err">
                                {e} " "
                                <button class="link-btn" on:click=move |_| ctx.bump()>
                                    "Retry"
                                </button>
                            </div>
                        }
                    })
            }}
            <Show when=move || ctx.kb.with(Option::is_some)>
                <div class="kb-stack">
                    <KbFacts/>
                    {move || match tab.get() {
                        "search" => view! { <super::knowledge_search::SearchTab/> }.into_any(),
                        "settings" => view! { <super::knowledge_settings::SettingsTab/> }.into_any(),
                        _ => view! { <super::knowledge_files::FilesTab/> }.into_any(),
                    }}
                </div>
            </Show>
        </PageFrame>
    }
}

/// The base's notes and its embedding pin, above every tab: what the owner
/// should know before trusting an answer from it.
#[component]
fn KbFacts() -> impl IntoView {
    let ctx = use_kb();
    view! {
        {move || {
            ctx.kb
                .get()
                .map(|kb| {
                    let limit = match kb.embed_input_limit {
                        Some(n) => format!("{n} tokens per input"),
                        None => "per-input limit unknown".to_string(),
                    };
                    view! {
                        <div class="kb-facts dim mono-sm">
                            <span title="the model this base is pinned to">
                                "embedding " {kb.embed_identity.clone()}
                            </span>
                            <span title=kb.embed_input_limit_source.clone()>{limit}</span>
                            <span>
                                {grouped(kb.counts.chunks as u64)} " chunks · "
                                {human_bytes(kb.resident_bytes.max(0) as u64)} " resident"
                            </span>
                        </div>
                        <For each=move || kb.notes.clone() key=|n| n.clone() let:n>
                            <div class="notice warn">{n}</div>
                        </For>
                    }
                })
        }}
    }
}
