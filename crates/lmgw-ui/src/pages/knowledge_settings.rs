//! The Settings tab: edit the base. A different embedding model re-pins it
//! and re-embeds every chunk; a different chunk size re-chunks every file —
//! both are said before Save, in the Save button's own words. Deleting the
//! base is two clicks and then really happens.

use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::fmt::grouped;
use crate::widgets::{use_toasts, ConfirmButton, Section};

use super::knowledge::use_kb;
use super::knowledge_form::{KbFields, KbForm};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct EditOutcome {
    reembed_job: Option<i64>,
    ingest_job: Option<i64>,
    rechunk_files: u64,
}

/// What the toast says the edit started, read from the edit response. A model
/// change starts ONE job whose first stage measures the stored chunks against
/// the new model and decides: re-embed in place, or re-chunk the files whose
/// chunks overrun it — so the toast does not promise either.
fn edit_toast(o: &EditOutcome) -> String {
    match (o.reembed_job, o.ingest_job) {
        (Some(_), Some(_)) => format!(
            "saved — a job measures the chunks against the new model and re-embeds or \
             re-chunks as needed, and {} files are re-chunked for the new size; progress below",
            o.rechunk_files
        ),
        (Some(_), None) => "saved — a job measures the chunks against the new model and \
                            re-embeds or re-chunks as needed; progress below"
            .to_string(),
        (None, Some(_)) => format!(
            "saved — re-chunking {} file{}; progress below",
            o.rechunk_files,
            if o.rechunk_files == 1 { "" } else { "s" }
        ),
        (None, None) => "saved".to_string(),
    }
}

#[component]
pub fn SettingsTab() -> impl IntoView {
    let ctx = use_kb();
    // The form starts from the base as it is when the tab opens; a reload
    // (a job ticking) must not overwrite what is being typed.
    let Some(start) = ctx.kb.get_untracked() else {
        return ().into_any();
    };
    let form = KbForm::of(&start);
    let toasts = use_toasts();
    let navigate = use_navigate();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let kb = move || ctx.kb.get();
    let model_changed = Memo::new(move |_| {
        kb().is_some_and(|k| {
            form.embed.get().trim() != k.embed_alias && !form.embed.get().trim().is_empty()
        })
    });
    let chunk_changed = Memo::new(move |_| {
        kb().is_some_and(|k| {
            form.chunk_tokens.get().trim() != k.chunk_tokens.to_string()
                || form.chunk_overlap.get().trim() != k.chunk_overlap.to_string()
        })
    });
    let limit = Signal::derive(move || {
        kb().map(|k| (k.embed_input_limit, k.embed_input_limit_source))
            .unwrap_or((None, String::new()))
    });

    let save = move |_| {
        if busy.get_untracked() {
            return;
        }
        let body = match form.body() {
            Ok(b) => b,
            Err(e) => {
                error.set(Some(e));
                return;
            }
        };
        busy.set(true);
        error.set(None);
        let id = ctx.id;
        leptos::task::spawn_local(async move {
            let res = crate::api::post::<EditOutcome, _>(
                format!("/api/knowledge/bases/{id}/settings"),
                &body,
            )
            .await;
            match res {
                Ok(o) => {
                    toasts.ok(edit_toast(&o));
                    if ctx.scope.alive() {
                        busy.set(false);
                        ctx.bump();
                    }
                }
                Err(e) => {
                    if ctx.scope.alive() {
                        busy.set(false);
                        error.set(Some(e.to_string()));
                    }
                }
            }
        });
    };

    let delete = Callback::new(move |()| {
        let id = ctx.id;
        let navigate = navigate.clone();
        leptos::task::spawn_local(async move {
            match crate::api::post::<Value, _>(
                format!("/api/knowledge/bases/{id}/delete"),
                &json!({}),
            )
            .await
            {
                Ok(_) => {
                    toasts.ok("knowledge base deleted");
                    navigate("/knowledge", Default::default());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    });

    view! {
        <div class="card">
            <KbFields form=form limit=limit/>
            <Show when=move || model_changed.get()>
                <div class="notice warn" style="margin-top:12px">
                    "Changing the embedding model re-embeds every chunk ("
                    {move || kb().map(|k| grouped(k.counts.chunks as u64)).unwrap_or_default()}
                    " of them). Until it finishes, searches on this base use keywords only."
                </div>
            </Show>
            <Show when=move || chunk_changed.get()>
                <div class="notice warn" style="margin-top:12px">
                    "Changing the chunk size re-chunks every file ("
                    {move || kb().map(|k| grouped(k.counts.files as u64)).unwrap_or_default()}
                    " of them): each is read again and only regions that changed are embedded anew."
                </div>
            </Show>
            {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
            <div class="row" style="margin-top:14px">
                <button class="btn primary" disabled=move || busy.get() on:click=save>
                    {move || {
                        match (model_changed.get(), chunk_changed.get()) {
                            _ if busy.get() => "Saving…",
                            (true, true) => "Save, re-chunk and re-embed",
                            (true, false) => "Save and re-embed",
                            (false, true) => "Save and re-chunk",
                            _ => "Save",
                        }
                    }}
                </button>
            </div>
        </div>
        <Section title="Delete this knowledge base" default_open=false>
            <div class="row">
                <span class="dim">
                    "Removes its files, chunks and uploaded originals. Chats that cited it keep their stored excerpts."
                </span>
                <ConfirmButton
                    label="Delete knowledge base"
                    confirm=move || {
                        kb().map(|k| {
                                format!(
                                    "Delete '{}' with {} files and {} chunks?",
                                    k.name,
                                    k.counts.files,
                                    k.counts.chunks,
                                )
                            })
                            .unwrap_or_else(|| "Delete?".into())
                    }
                    on_confirm=delete
                    class="btn danger"
                />
            </div>
        </Section>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(reembed: Option<i64>, ingest: Option<i64>, files: u64) -> EditOutcome {
        EditOutcome {
            reembed_job: reembed,
            ingest_job: ingest,
            rechunk_files: files,
        }
    }

    #[test]
    fn the_toast_says_what_the_edit_started() {
        assert_eq!(edit_toast(&out(None, None, 0)), "saved");
        let model = edit_toast(&out(Some(4), None, 0));
        assert!(model.contains("measures the chunks"), "{model}");
        assert!(!model.contains("every chunk"), "{model}");
        assert_eq!(
            edit_toast(&out(None, Some(5), 1)),
            "saved — re-chunking 1 file; progress below"
        );
        assert!(edit_toast(&out(Some(4), Some(5), 3)).contains("3 files"));
    }
}
