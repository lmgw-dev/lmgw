//! The Files tab: what the base holds as a boxed list (a row per file: its
//! name, kind, size, pages, skipped pages and chunks, its state and why, and
//! a menu with View text, Download original, Re-ingest and Delete), files
//! dropped anywhere on the group or picked with Choose files, what became of
//! each upload, and the base's work in progress as rows (the live job with
//! Cancel, Resume when work waits).

use leptos::prelude::*;
use serde_json::{json, Value};
use wasm_bindgen::JsValue;

use crate::fmt::{fmt_bytes, grouped};
use crate::widgets::{use_toasts, ActionRow, Ico, Icon, MenuItem, PrefGroup, RowMenu};

use super::knowledge::{use_kb, KbFile, KbJobRow};
use lmgw_chat_ui::knowledge_source::{SourceModal, SourceRef};

use lmgw_api_types::knowledge::{UploadItem, UploadResult as UploadResponse, UploadVerdict};

fn drag_has_files(dt: &web_sys::DataTransfer) -> bool {
    dt.types().includes(&JsValue::from_str("Files"), 0)
}

/// One toast for the whole upload: how many of each outcome.
fn summary(items: &[UploadItem]) -> String {
    let mut parts = Vec::new();
    for o in [
        UploadVerdict::Added,
        UploadVerdict::Replaced,
        UploadVerdict::Unchanged,
        UploadVerdict::Duplicate,
        UploadVerdict::Refused,
    ] {
        let n = items.iter().filter(|i| i.outcome == o).count();
        if n > 0 {
            parts.push(format!("{n} {}", o.as_str()));
        }
    }
    parts.join(", ")
}

/// A file's second line: what it is and what came of it.
fn file_meta(f: &KbFile) -> String {
    let mut parts = Vec::new();
    let mut kind = f.kind.clone();
    if !f.sub.is_empty() && f.sub != f.kind {
        kind = format!("{kind} · {}", f.sub);
    }
    if !kind.is_empty() {
        parts.push(kind);
    }
    parts.push(fmt_bytes(f.size.max(0) as u64));
    if let Some(p) = f.pages {
        parts.push(format!("{p} page{}", if p == 1 { "" } else { "s" }));
    }
    if f.skipped_pages > 0 {
        parts.push(format!("{} skipped", grouped(f.skipped_pages as u64)));
    }
    parts.push(format!("{} chunks", grouped(f.chunk_count.max(0) as u64)));
    parts.join(" · ")
}

#[component]
pub fn FilesTab() -> impl IntoView {
    let ctx = use_kb();
    let toasts = use_toasts();
    let busy = RwSignal::new(false);
    let over = RwSignal::new(false);
    let results = RwSignal::new(Vec::<UploadItem>::new());
    let source = RwSignal::new(None::<SourceRef>);
    let input: NodeRef<leptos::html::Input> = NodeRef::new();

    let upload = move |files: web_sys::FileList| {
        if files.length() == 0 || busy.get_untracked() {
            return;
        }
        let Ok(form) = web_sys::FormData::new() else {
            toasts.err("this browser has no FormData");
            return;
        };
        for i in 0..files.length() {
            if let Some(f) = files.get(i) {
                let _ = form.append_with_blob_and_filename("file", &f, &f.name());
            }
        }
        busy.set(true);
        leptos::task::spawn_local(async move {
            let res = crate::api::post_raw::<UploadResponse>(
                format!("/api/knowledge/bases/{}/files", ctx.id),
                form,
            )
            .await;
            match res {
                Ok(r) => {
                    let s = summary(&r.items);
                    if r.items.iter().any(|i| i.outcome == UploadVerdict::Refused) {
                        toasts.warn(s);
                    } else {
                        toasts.ok(s);
                    }
                    if ctx.scope.alive() {
                        results.set(r.items);
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
            if ctx.scope.alive() {
                busy.set(false);
                ctx.bump();
            }
        });
    };

    let pick = move |_| {
        if let Some(i) = input.get() {
            if let Some(files) = i.files() {
                upload(files);
            }
            // The same file picked again is a new change.
            i.set_value("");
        }
    };

    let has_job = Memo::new(move |_| ctx.page.has_job(ctx.id));
    let needs_resume = Memo::new(move |_| {
        !has_job.get()
            && ctx.kb.with(|k| {
                k.as_ref().is_some_and(|k| {
                    k.counts.pending > 0
                        || k.counts.embedded < k.counts.chunks
                        || k.status == "re_embed_required"
                })
            })
    });

    view! {
        <Show when=move || has_job.get() || needs_resume.get()>
            <PrefGroup title="In progress">
                <KbJobRow id=ctx.id/>
                <Show when=move || needs_resume.get()>
                    <ActionRow
                        title="Resume"
                        sub="Work is waiting: files to ingest, or chunks without a vector. Start it again."
                    >
                        <button
                            type="button"
                            class="btn suggested pill"
                            on:click=move |_| super::knowledge::resume_kb(toasts, ctx)
                        >
                            "Resume"
                        </button>
                    </ActionRow>
                </Show>
            </PrefGroup>
        </Show>

        <Show when=move || !results.with(Vec::is_empty)>
            <PrefGroup
                title="Last upload"
                suffix=move || {
                    view! {
                        <button class="btn flat sm" on:click=move |_| results.set(Vec::new())>
                            "Dismiss"
                        </button>
                    }
                }
            >
                <div class="kb-results">
                    <For each=move || results.get() key=|i| (i.name.clone(), i.outcome, i.reason.clone()) let:i>
                        <div class="prow kb-result">
                            <div class="prow-text">
                                <span class="prow-title" title=i.name.clone()>{i.name.clone()}</span>
                                {i.reason.clone().map(|r| view! { <span class="prow-sub">{r}</span> })}
                            </div>
                            <span class=match i.outcome.as_str() {
                                "added" | "replaced" => "chip ok",
                                "refused" => "chip err",
                                _ => "chip off",
                            }>{i.outcome.as_str()}</span>
                        </div>
                    </For>
                </div>
            </PrefGroup>
        </Show>

        <div
            class="kb-drop"
            class:drag-over=move || over.get()
            on:dragover=move |ev| {
                if ev.data_transfer().is_some_and(|dt| drag_has_files(&dt)) {
                    ev.prevent_default();
                    over.set(true);
                }
            }
            on:dragleave=move |ev| {
                ev.prevent_default();
                over.set(false);
            }
            on:drop=move |ev| {
                let Some(dt) = ev.data_transfer() else { return };
                if !drag_has_files(&dt) {
                    return;
                }
                ev.prevent_default();
                over.set(false);
                if let Some(files) = dt.files() {
                    upload(files);
                }
            }
        >
            <PrefGroup
                title=Signal::derive(move || {
                    let n = ctx.files.with(Vec::len);
                    format!("Files · {}", grouped(n as u64))
                })
            >
                <div class="prow kb-add">
                    <Icon ico=Ico::Upload class="kb-add-ico"/>
                    <div class="prow-text">
                        <span class="prow-title">
                            {move || if busy.get() { "Uploading…" } else { "Add files" }}
                        </span>
                        <span class="prow-sub">
                            "Drop them here, or choose them: PDF, Office, text, Markdown, spreadsheets, code"
                        </span>
                    </div>
                    <input node_ref=input type="file" multiple hidden on:change=pick/>
                    <button
                        type="button"
                        class="btn"
                        disabled=move || busy.get()
                        on:click=move |_| {
                            if let Some(i) = input.get() {
                                i.click();
                            }
                        }
                    >
                        "Choose files…"
                    </button>
                </div>
                <div class="kb-files">
                    <For
                        each=move || ctx.files.get()
                        key=|f| (f.id, f.status.clone(), f.chunk_count, f.error.clone(), f.size, f.notes.clone())
                        let:f
                    >
                        <FileRow f=f source=source/>
                    </For>
                </div>
                <Show when=move || ctx.files.with(Vec::is_empty)>
                    <div class="prow kb-none">"No files yet."</div>
                </Show>
            </PrefGroup>
        </div>
        <SourceModal target=source/>
    }
}

#[component]
fn FileRow(f: KbFile, source: RwSignal<Option<SourceRef>>) -> impl IntoView {
    let ctx = use_kb();
    let toasts = use_toasts();
    let id = f.id;
    let name = f.name.clone();
    let post = move |what: &'static str, done: &'static str| {
        leptos::task::spawn_local(async move {
            match crate::api::post::<Value, _>(
                format!("/api/knowledge/files/{id}/{what}"),
                &json!({}),
            )
            .await
            {
                Ok(_) => toasts.ok(done),
                Err(e) => toasts.err(e.to_string()),
            }
            if ctx.scope.alive() {
                ctx.bump();
            }
        });
    };
    let items = {
        let name = name.clone();
        let busy = f.status == "ingesting";
        Signal::derive(move || {
            let name = name.clone();
            vec![
                MenuItem::new("View text", move || {
                    source.set(Some(SourceRef {
                        file_id: id,
                        ..Default::default()
                    }))
                }),
                MenuItem::new("Download original", move || {
                    super::docs::download(&format!("/api/knowledge/files/{id}/original"), &name)
                }),
                MenuItem::new("Re-ingest", move || {
                    post("reingest", "queued for re-ingest")
                })
                .disabled(busy),
                MenuItem::new("Delete", move || post("delete", "file deleted"))
                    .danger()
                    .disabled(busy),
            ]
        })
    };
    let chip = match f.status.as_str() {
        "ready" => "chip ok",
        "failed" => "chip err",
        "ingesting" => "chip info",
        _ => "chip warn",
    };
    let meta = file_meta(&f);
    let reason = f.error.clone();
    let notes = f.notes.clone();
    view! {
        <div class="prow kb-file" data-file=f.name.clone()>
            <div class="prow-text">
                <button
                    type="button"
                    class="link-btn kb-file-name"
                    title=format!("{} · view the extracted text", f.name)
                    on:click=move |_| source.set(Some(SourceRef { file_id: id, ..Default::default() }))
                >
                    {f.name.clone()}
                </button>
                <span class="prow-sub kb-file-meta" title=f.mime.clone()>{meta}</span>
                {reason.map(|r| view! { <span class="prow-sub kb-reason">{r}</span> })}
                {notes
                    .into_iter()
                    .map(|n| view! { <span class="prow-sub">{n}</span> })
                    .collect_view()}
            </div>
            <div class="prow-end">
                <span class=chip>
                    <span class="dot"></span>
                    {f.status.clone()}
                </span>
                <RowMenu items=items/>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_row_says_what_it_is() {
        let f = KbFile {
            kind: "pdf".into(),
            size: 2048,
            pages: Some(2),
            skipped_pages: 1,
            chunk_count: 7,
            ..Default::default()
        };
        assert_eq!(
            file_meta(&f),
            "pdf · 2.0 KiB · 2 pages · 1 skipped · 7 chunks"
        );
        let g = KbFile {
            kind: "text".into(),
            sub: "markdown".into(),
            size: 10,
            ..Default::default()
        };
        assert_eq!(file_meta(&g), "text · markdown · 10 B · 0 chunks");
    }
}
