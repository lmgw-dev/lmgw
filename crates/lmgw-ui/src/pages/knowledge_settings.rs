//! The Settings tab: the base's fields as preference rows, a changed one
//! with the accent dot, the save toast with Ctrl+S, and leaving with
//! unsaved changes asks first. A different embedding model re-pins it and
//! re-embeds every chunk; a different chunk size re-chunks every file —
//! both are said before Save, in notes and in the Save button's own words.
//! Delete asks first and then really happens.

use leptos::prelude::*;

use crate::fmt::grouped;
use crate::widgets::nav_split::SaveKey;
use crate::widgets::{use_toasts, ActionRow, PrefGroup, SaveToast};

use super::knowledge::{use_kb, Kb};
use super::knowledge_form::{KbDirty, KbFields, KbForm, KbValues};

use lmgw_api_types::knowledge::KnowledgeBaseEdited as EditOutcome;

/// What the toast says the edit started, read from the edit response. A model
/// change starts ONE job whose first stage measures the stored chunks against
/// the new model and decides: re-embed in place, or re-chunk the files whose
/// chunks overrun it — so the toast does not promise either.
fn edit_toast(o: &EditOutcome) -> String {
    match (o.reembed_job, o.ingest_job) {
        (Some(_), Some(_)) => format!(
            "saved — a job measures the chunks against the new model and re-embeds or \
             re-chunks as needed, and {} files are re-chunked for the new size; progress on Files",
            o.rechunk_files
        ),
        (Some(_), None) => "saved — a job measures the chunks against the new model and \
                            re-embeds or re-chunks as needed; progress on Files"
            .to_string(),
        (None, Some(_)) => format!(
            "saved — re-chunking {} file{}; progress on Files",
            o.rechunk_files,
            if o.rechunk_files == 1 { "" } else { "s" }
        ),
        (None, None) => "saved".to_string(),
    }
}

/// The Save button's words for what the save will start.
fn save_words(model: bool, chunks: bool) -> &'static str {
    match (model, chunks) {
        (true, true) => "Save, re-chunk and re-embed",
        (true, false) => "Save and re-embed",
        (false, true) => "Save and re-chunk",
        _ => "Save",
    }
}

/// A model change the server takes only once its re-embed job runs: the job
/// probes the model and then re-pins the base, so the reads in between
/// still name the old model. The job's id and the model sent.
#[derive(Clone, Debug, PartialEq)]
struct Repin {
    job: i64,
    alias: String,
}

/// Whether the read `kb` ends the wait: it names the model sent, or the job
/// has ended (a failed or canceled one leaves the pin as the server says),
/// or a later job is on the base.
fn repin_settled(r: &Repin, kb: &Kb) -> bool {
    kb.embed_alias.trim() == r.alias.trim()
        || kb.job.as_ref().is_some_and(|j| {
            j.id > r.job || (j.id == r.job && !matches!(j.status.as_str(), "queued" | "running"))
        })
}

/// The base as the form takes it: `kb`, with the model sent in place of the
/// one it still names while its re-pin is pending.
fn baseline_of(kb: &Kb, held: Option<&Repin>) -> KbValues {
    let mut v = KbValues::of(kb);
    if let Some(r) = held {
        v.embed = r.alias.clone();
    }
    v
}

#[component]
pub fn SettingsTab() -> impl IntoView {
    let ctx = use_kb();
    // The form starts from the base as it is when the tab opens. Every read
    // after that moves its baseline: a field the owner has not changed
    // follows (someone else's edit shows), one being typed stays as typed.
    let Some(start) = ctx.kb.get_untracked() else {
        return ().into_any();
    };
    let form = KbForm::of(&start);
    let toasts = use_toasts();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // A saved model change the reads do not show yet (see [`Repin`]).
    let held = RwSignal::new(None::<Repin>);
    Effect::new(move |_| {
        let Some(k) = ctx.kb.get() else {
            return;
        };
        if held.with_untracked(|h| h.as_ref().is_some_and(|r| repin_settled(r, &k))) {
            held.set(None);
        }
        form.merge(held.with_untracked(|h| baseline_of(&k, h.as_ref())));
    });

    let kb = move || ctx.kb.get();
    let dirty = Memo::new(move |_| form.dirty());
    let dirty_count = Signal::derive(move || dirty.get().count());
    let invalid = Signal::derive(move || form.invalid());
    let model_changed =
        Memo::new(move |_| dirty.get().embed && !form.embed.with(|e| e.trim().is_empty()));
    let chunk_changed = Memo::new(move |_| {
        let d: KbDirty = dirty.get();
        d.chunk_tokens || d.chunk_overlap
    });
    let limit = Signal::derive(move || {
        kb().map(|k| (k.embed_input_limit, k.embed_input_limit_source))
            .unwrap_or((None, String::new()))
    });

    let id = ctx.id;
    ctx.page.guard().watch_in(
        "this knowledge base",
        Signal::derive(move || dirty_count.get() > 0),
        Signal::stored(format!("/knowledge/{id}/settings")),
    );

    let on_save = Callback::new(move |()| {
        if busy.get_untracked() || dirty_count.get_untracked() == 0 || invalid.get_untracked() > 0 {
            return;
        }
        let sent = form.snapshot();
        let body = match form.body() {
            Ok(b) => b,
            Err(e) => {
                error.set(Some(e));
                return;
            }
        };
        busy.set(true);
        error.set(None);
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
                        // What was sent is saved, and the rows say so now;
                        // typing since the body was built stays a change.
                        held.set(o.reembed_job.map(|job| Repin {
                            job,
                            alias: sent.embed.trim().to_string(),
                        }));
                        form.rebase(sent);
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
    });
    let on_discard = Callback::new(move |()| {
        form.discard();
        error.set(None);
    });
    SaveKey::install(on_save);

    view! {
        <KbFields form=form limit=limit dirty=Signal::from(dirty)/>
        <Show when=move || model_changed.get() || chunk_changed.get()>
            <PrefGroup title="What Save starts">
                <Show when=move || model_changed.get()>
                    <div class="prow notice-row warn">
                        "Changing the embedding model re-embeds every chunk ("
                        {move || kb().map(|k| grouped(k.counts.chunks.max(0) as u64)).unwrap_or_default()}
                        " of them). Until it finishes, searches on this base use keywords only."
                    </div>
                </Show>
                <Show when=move || chunk_changed.get()>
                    <div class="prow notice-row warn">
                        "Changing the chunk size re-chunks every file ("
                        {move || kb().map(|k| grouped(k.counts.files.max(0) as u64)).unwrap_or_default()}
                        " of them): each is read again and only regions that changed are embedded anew."
                    </div>
                </Show>
            </PrefGroup>
        </Show>
        <PrefGroup>
            <ActionRow
                title="Delete knowledge base…"
                sub="Removes its files, chunks and uploaded originals. Chats that cited it keep their stored excerpts."
                danger=true
                on_click=Callback::new(move |()| ctx.deleting.set(true))
            />
        </PrefGroup>
        <SaveToast
            dirty_count=dirty_count
            invalid=invalid
            saving=busy
            error=error
            on_save=on_save
            on_discard=on_discard
            save_label=move || save_words(model_changed.get(), chunk_changed.get())
        />
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
            ..Default::default()
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
            "saved — re-chunking 1 file; progress on Files"
        );
        assert!(edit_toast(&out(Some(4), Some(5), 3)).contains("3 files"));
    }

    fn base(alias: &str, job: Option<(i64, &str)>) -> Kb {
        Kb {
            name: "Taxes".into(),
            embed_alias: alias.into(),
            chunk_tokens: 512,
            chunk_overlap: 64,
            mcp_visible: true,
            job: job.map(|(id, status)| lmgw_api_types::JobRow {
                id,
                kind: "kb_reembed".into(),
                status: status.into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The typed values and the baseline after the read `kb`, the baseline
    /// having been `was`.
    fn after_read(
        typed: &KbValues,
        was: &KbValues,
        kb: &Kb,
        held: &mut Option<Repin>,
    ) -> (KbValues, KbValues) {
        if held.as_ref().is_some_and(|r| repin_settled(r, kb)) {
            *held = None;
        }
        let now = baseline_of(kb, held.as_ref());
        (typed.follow(was, &now), now)
    }

    #[test]
    fn a_saved_model_stays_saved_while_its_job_re_pins() {
        let mut typed = KbValues::of(&base("embed-a", None));
        typed.embed = "embed-b".into();
        let sent = typed.clone();
        let mut held = Some(Repin {
            job: 7,
            alias: "embed-b".into(),
        });
        // The read right after Save still names the old model.
        let (typed, now) = after_read(
            &typed,
            &sent,
            &base("embed-a", Some((7, "queued"))),
            &mut held,
        );
        assert!(held.is_some());
        assert_eq!(typed.embed, "embed-b");
        assert_eq!(typed.dirty_against(&now).count(), 0);
        // The job has re-pinned: nothing to save, no second re-embed.
        let (typed, now) = after_read(
            &typed,
            &now,
            &base("embed-b", Some((7, "running"))),
            &mut held,
        );
        assert!(held.is_none());
        assert_eq!(typed.embed, "embed-b");
        assert_eq!(typed.dirty_against(&now).count(), 0);
    }

    #[test]
    fn a_failed_re_pin_shows_the_model_the_base_kept() {
        let mut typed = KbValues::of(&base("embed-a", None));
        typed.embed = "embed-b".into();
        let sent = typed.clone();
        let mut held = Some(Repin {
            job: 7,
            alias: "embed-b".into(),
        });
        let (typed, now) = after_read(
            &typed,
            &sent,
            &base("embed-a", Some((7, "failed"))),
            &mut held,
        );
        assert!(held.is_none());
        assert_eq!(typed.embed, "embed-a");
        assert_eq!(typed.dirty_against(&now).count(), 0);
    }

    #[test]
    fn a_later_change_still_shows_and_typing_stays() {
        let mut typed = KbValues::of(&base("embed-a", None));
        typed.embed = "embed-b".into();
        let sent = typed.clone();
        let mut held = Some(Repin {
            job: 7,
            alias: "embed-b".into(),
        });
        // Typed after the body was built: still a change.
        typed.description = "receipts".into();
        assert_eq!(typed.dirty_against(&sent).count(), 1);
        // Someone else renames the base meanwhile: the name follows.
        let mut renamed = base("embed-a", Some((7, "running")));
        renamed.name = "Taxes 2026".into();
        let (typed, now) = after_read(&typed, &sent, &renamed, &mut held);
        assert_eq!(typed.name, "Taxes 2026");
        assert_eq!(typed.embed, "embed-b");
        let d = typed.dirty_against(&now);
        assert!(d.description && d.count() == 1, "{d:?}");
    }

    #[test]
    fn save_says_what_it_starts() {
        assert_eq!(save_words(false, false), "Save");
        assert_eq!(save_words(true, false), "Save and re-embed");
        assert_eq!(save_words(false, true), "Save and re-chunk");
        assert_eq!(save_words(true, true), "Save, re-chunk and re-embed");
    }
}
