//! The detail pane: the selected base under a header bar with its tabs
//! (Files, Search, Settings) and a menu (Resume, Cancel the job, Delete),
//! then the hero — the base's name and description, chips for its state,
//! files, size, chunks, what is resident and the model it is pinned to —
//! the notes the owner should read before trusting an answer from it, and
//! the tab. With nothing selected it is a status page; a path naming no
//! base says so.

use leptos::prelude::*;
use serde_json::{json, Value};

use super::{kb_href, status_of, use_kb, use_kb_page, DetailResponse, Kb, KbCtx, KB_TABS};
use crate::fmt::{fmt_bytes, grouped};
use crate::scope::Scope;
use crate::widgets::nav_split::{track_scroll, DetailHead};
use crate::widgets::{
    use_toasts, ActionRow, Dot, Ico, Icon, InfoRow, MenuItem, Modal, ModalFooter, NavTab,
    PrefGroup, StateChip, SubNav, Tone,
};

#[component]
pub(super) fn DetailPane(
    raw: Memo<Option<String>>,
    sel: Memo<Option<i64>>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    // The id is the detail's identity: another one is a new detail, so
    // nothing an answer for the last one brings lands on it.
    move || match (sel.get(), raw.with(Option::is_some)) {
        (Some(id), _) => view! { <DetailBody id=id narrow=narrow back=back/> }.into_any(),
        (None, true) => view! { <Gone narrow=narrow back=back/> }.into_any(),
        (None, false) => view! { <StatusPage narrow=narrow back=back/> }.into_any(),
    }
}

#[component]
fn DetailBody(id: i64, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let page = use_kb_page();
    let toasts = use_toasts();
    let ctx = KbCtx {
        id,
        kb: RwSignal::new(None),
        files: RwSignal::new(Vec::new()),
        reload: RwSignal::new(0),
        scope: Scope::new(),
        deleting: RwSignal::new(false),
        page,
    };
    provide_context(ctx);
    let error = RwSignal::new(None::<String>);
    let missing = RwSignal::new(false);
    let tab = crate::url_state::use_view("tab", KB_TABS, "files");

    let res = LocalResource::new(move || {
        ctx.reload.get();
        crate::api::get::<DetailResponse>(format!("/api/knowledge/bases/{id}"))
    });
    Effect::new(move |_| match res.get() {
        Some(Ok(v)) => {
            page.put(&v.base);
            ctx.kb.set(Some(v.base));
            ctx.files.set(v.files);
            error.set(None);
        }
        Some(Err(e)) => {
            // A base the list does not have (deleted, or never was) is
            // gone, not an error to retry.
            let listed = page.bases.with(|b| b.iter().any(|k| k.id == id));
            if page.loaded.get_untracked() && !listed && ctx.kb.with_untracked(Option::is_none) {
                missing.set(true);
            }
            error.set(Some(e.to_string()));
        }
        None => {}
    });
    super::reload_on_own_job(page, id, ctx.reload);

    let tabs = Signal::derive(move || {
        let mut f = NavTab::new("Files", kb_href(id, "files")).exact();
        if let Some(n) = ctx.kb.with(|k| k.as_ref().map(|k| k.counts.files)) {
            f = f.count(n);
        }
        if ctx
            .kb
            .with(|k| k.as_ref().is_some_and(|k| k.counts.failed > 0))
        {
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
    let job = Memo::new(move |_| page.job(id));
    let needs_resume = Memo::new(move |_| {
        job.with(Option::is_none)
            && ctx.kb.with(|k| {
                k.as_ref().is_some_and(|k| {
                    k.counts.pending > 0
                        || k.counts.embedded < k.counts.chunks
                        || k.status == "re_embed_required"
                })
            })
    });
    let resume = move || super::resume_kb(toasts, ctx);
    let more = Signal::derive(move || {
        let mut v = Vec::new();
        if needs_resume.get() {
            v.push(MenuItem::new("Resume", resume).title("Start the waiting work again"));
        }
        if job.with(Option::is_some) {
            v.push(MenuItem::new("Cancel the job", move || {
                super::cancel_job(toasts, id)
            }));
        }
        v.push(
            MenuItem::new("Delete…", move || ctx.deleting.set(true))
                .title("Its files, chunks and uploaded originals"),
        );
        v
    });
    let scrolled = RwSignal::new(false);

    view! {
        {move || {
            if missing.get() {
                view! { <DetailHead narrow=narrow back=back title=title scrolled=scrolled/> }.into_any()
            } else {
                view! {
                    <DetailHead
                        narrow=narrow
                        back=back
                        title=title
                        scrolled=scrolled
                        tabs=move || view! { <SubNav tabs=tabs/> }
                        more=more
                    />
                }
                    .into_any()
            }
        }}
        <div
            class="detail-scroll mdetail-scroll"
            tabindex="-1"
            on:scroll=move |ev| track_scroll(&ev, scrolled)
        >
            {move || {
                missing
                    .get()
                    .then(|| {
                        view! {
                            <div class="detail-flow single">
                                <div class="mdetail-empty">
                                    <p>"This knowledge base is not here: it was deleted, or the link names one that never was."</p>
                                    <button type="button" class="btn" on:click=move |_| back.run(())>
                                        "Back to the list"
                                    </button>
                                </div>
                            </div>
                        }
                    })
            }}
            <div class="detail-flow kb-flow" class:single=move || tab.get() != "settings">
                {move || {
                    error
                        .get()
                        .filter(|_| !missing.get())
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
                    <Hero/>
                    {move || match tab.get() {
                        "search" => view! { <crate::pages::knowledge_search::SearchTab/> }.into_any(),
                        "settings" => {
                            view! { <crate::pages::knowledge_settings::SettingsTab/> }.into_any()
                        }
                        _ => view! { <crate::pages::knowledge_files::FilesTab/> }.into_any(),
                    }}
                </Show>
            </div>
        </div>
        <DeleteKbModal/>
    }
}

/// The base's name, description and chips, and its notes under them. Built
/// once; each piece follows the base as read and its job.
#[component]
fn Hero() -> impl IntoView {
    let ctx = use_kb();
    let page = ctx.page;
    let id = ctx.id;
    // One read of a field of the base, tracked.
    let of =
        move |f: fn(&Kb) -> String| move || ctx.kb.with(|k| k.as_ref().map(f).unwrap_or_default());
    let flag = move |f: fn(&Kb) -> bool| {
        Signal::derive(move || ctx.kb.with(|k| k.as_ref().is_some_and(f)))
    };
    let st = Memo::new(move |_| {
        let job = page.job(id);
        ctx.kb
            .with(|k| k.as_ref().map(|k| status_of(k, job.as_ref())))
    });
    let word = move || st.with(|s| s.as_ref().map(|s| s.1.clone()).unwrap_or_default());
    let dot = Signal::derive(move || st.with(|s| s.as_ref().map_or(Dot::Off, |s| s.0)));
    let why = move || st.with(|s| s.as_ref().map(|s| s.2.clone()).unwrap_or_default());
    let desc = Memo::new(move |_| {
        ctx.kb.with(|k| match k {
            Some(k) if !k.description.is_empty() => k.description.clone(),
            _ => "No description: models read it to pick a base".to_string(),
        })
    });
    let notes = Memo::new(move |_| {
        ctx.kb
            .with(|k| k.as_ref().map(|k| k.notes.clone()).unwrap_or_default())
    });
    let gone = flag(|k| !k.embed_resolvable);
    let hidden = flag(|k| !k.mcp_visible);
    view! {
        <div class="detail-hero kb-hero">
            <div class="glyph">
                <Icon ico=Ico::Box/>
            </div>
            <div class="hero-text">
                <h1>{of(|k| k.name.clone())}</h1>
                <div class="sub" title=move || desc.get()>{move || desc.get()}</div>
                <div class="chips kb-facts">
                    <StateChip dot=dot word=word title=why/>
                    <span class="chip">
                        {of(|k| {
                            let c = &k.counts;
                            format!(
                                "{} file{} · {}",
                                grouped(c.files.max(0) as u64),
                                if c.files == 1 { "" } else { "s" },
                                fmt_bytes(c.bytes.max(0) as u64),
                            )
                        })}
                    </span>
                    <span class="chip">
                        {of(|k| {
                            format!(
                                "{} chunks · {} resident",
                                grouped(k.counts.chunks.max(0) as u64),
                                fmt_bytes(k.resident_bytes.max(0) as u64),
                            )
                        })}
                    </span>
                    <span class="chip mono-chip" title="the model this base is pinned to">
                        {of(|k| k.embed_identity.clone())}
                    </span>
                    <span class="chip" title=of(|k| k.embed_input_limit_source.clone())>
                        {of(|k| match k.embed_input_limit {
                            Some(n) => format!("{} tokens per input", grouped(n)),
                            None => "Per-input limit unknown".to_string(),
                        })}
                    </span>
                    <Show when=move || gone.get()>
                        <span
                            class="chip err"
                            title="no model on this gateway resolves to this pin any more — searches fall back to keywords"
                        >
                            <span class="dot"></span>
                            "Model gone"
                        </span>
                    </Show>
                    <Show when=move || hidden.get()>
                        <span class="chip off" title="the kb__* tools on /mcp do not search it">
                            "Hidden from API clients"
                        </span>
                    </Show>
                </div>
            </div>
        </div>
        <Show when=move || notes.with(|n| !n.is_empty())>
            <div class="kb-hero-notes">
                {move || {
                    notes
                        .get()
                        .into_iter()
                        .map(|n| view! { <div class="notice warn">{n}</div> })
                        .collect_view()
                }}
            </div>
        </Show>
    }
}

/// Delete asks first, with what goes, and then really deletes.
#[component]
fn DeleteKbModal() -> impl IntoView {
    let ctx = use_kb();
    let toasts = use_toasts();
    let busy = RwSignal::new(false);
    let delete = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        let id = ctx.id;
        let page = ctx.page;
        leptos::task::spawn_local(async move {
            match crate::api::post::<Value, _>(
                format!("/api/knowledge/bases/{id}/delete"),
                &json!({}),
            )
            .await
            {
                Ok(_) => {
                    toasts.ok("knowledge base deleted");
                    page.bump();
                    // Its unsaved settings have no base left to go to.
                    page.go_unasked("/knowledge", false);
                }
                Err(e) => {
                    toasts.err(e.to_string());
                    let _ = busy.try_set(false);
                }
            }
        });
    };
    view! {
        <Modal open=ctx.deleting title="Delete knowledge base">
            <p>
                {move || {
                    ctx.kb
                        .get()
                        .map(|k| {
                            format!(
                                "Delete \u{201c}{}\u{201d} with {} files and {} chunks? Its files, chunks and uploaded originals go. Chats that cited it keep their stored excerpts.",
                                k.name,
                                grouped(k.counts.files.max(0) as u64),
                                grouped(k.counts.chunks.max(0) as u64),
                            )
                        })
                        .unwrap_or_default()
                }}
            </p>
            <ModalFooter>
                <button class="btn" on:click=move |_| ctx.deleting.set(false)>
                    "Keep it"
                </button>
                <button class="btn destructive pill" disabled=move || busy.get() on:click=delete>
                    {move || if busy.get() { "Deleting…" } else { "Delete knowledge base" }}
                </button>
            </ModalFooter>
        </Modal>
    }
}

/// Nothing selected: what the list holds, and the way to add.
#[component]
fn StatusPage(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let page = use_kb_page();
    let scrolled = RwSignal::new(false);
    let sum = move |f: fn(&super::Kb) -> i64| {
        page.bases
            .with(|b| b.iter().map(f).sum::<i64>().max(0) as u64)
    };
    view! {
        <DetailHead narrow=narrow back=back title="Knowledge" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow kb-status">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Box/>
                    </div>
                    <div class="hero-text">
                        <h1>"Knowledge"</h1>
                        <div class="sub">
                            "Your documents, in collections: searched from Chat and served on /mcp. Pick one in the list, or make one."
                        </div>
                    </div>
                </div>
                <PrefGroup title="In the list">
                    <InfoRow title="Knowledge bases" value=move || grouped(page.bases.with(Vec::len) as u64)/>
                    <InfoRow
                        title="Files"
                        value=move || {
                            format!(
                                "{} · {}",
                                grouped(sum(|k| k.counts.files)),
                                fmt_bytes(sum(|k| k.counts.bytes)),
                            )
                        }
                    />
                    <InfoRow title="Chunks" value=move || grouped(sum(|k| k.counts.chunks))/>
                    <InfoRow title="Resident" value=move || fmt_bytes(sum(|k| k.resident_bytes))/>
                </PrefGroup>
                <PrefGroup title="Add">
                    <ActionRow
                        title="New knowledge base"
                        sub="Name it, pick the embedding model, then drop your files into it"
                        chevron=true
                        on_click=Callback::new(move |()| page.new_open.set(true))
                    />
                </PrefGroup>
            </div>
        </div>
    }
}

/// A path whose id is no knowledge base id.
#[component]
fn Gone(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    view! {
        <DetailHead narrow=narrow back=back title="Not here" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll">
            <div class="mdetail-empty">
                <p>"That is not a knowledge base id."</p>
                <button type="button" class="btn" on:click=move |_| back.run(())>
                    "Back to the list"
                </button>
            </div>
        </div>
    }
}
