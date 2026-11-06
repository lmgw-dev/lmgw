//! The fit note and "Fit again" in a model's editor (find-a-model design
//! D26): a row whose context Get it fitted to the graphics card says so,
//! with the context it got, the model's own, what it was fitted to and
//! when, and "Fit again" takes the fit on the card as it is now. The
//! changes it would make are shown first; "Save" posts them as shown, and
//! when the card moved past them meanwhile, or the row changed, nothing is
//! written and the new changes are shown instead.
//!
//! The note comes with the row's read (`fit_note`), so it costs no write
//! to show; the editor's own save reads the row again, and a save of
//! another context drops the note there. A row without a note shows
//! nothing here yet: Fit again is offered where a fit set the context.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::model_find::{FitMeasure, FitNote};
use lmgw_api_types::model_get::{FitChange, ModelFitAgain, ModelFitAgainArgs};

use crate::widgets::use_toasts;

/// `model_fit_again`: `confirm` `None` only says what it would change.
async fn post(
    target: &str,
    model_id: &str,
    confirm: Option<Vec<FitChange>>,
) -> Result<ModelFitAgain, String> {
    crate::api::post::<ModelFitAgain, _>(
        "/api/op/model_fit_again",
        &ModelFitAgainArgs {
            target: target.to_string(),
            model_id: model_id.to_string(),
            confirm,
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// The note in words: "Remembers 32 768 of the model's 262 144 tokens:
/// fitted beside what ran on the graphics card (7.2 GiB to use) on
/// 2026-11-04 09:12 UTC."
pub(crate) fn note_line(n: &FitNote) -> String {
    let tokens = crate::fmt::grouped(n.ctx);
    let of = n
        .trained_ctx
        .map(|t| format!(" of the model's {}", crate::fmt::grouped(t)))
        .unwrap_or_default();
    let measure = match n.measure {
        FitMeasure::Beside => "beside what ran on the graphics card",
        FitMeasure::WholeCard => "to the whole graphics card",
    };
    let at = n.at.get(..16).unwrap_or(&n.at);
    format!(
        "Remembers {tokens}{of} tokens: fitted {measure} ({} to use) on {at} UTC.",
        crate::fmt::fmt_bytes(n.capacity_bytes)
    )
}

/// The change list in words, by the editor's own labels: "Context size
/// 32 768 → 65 536 · Physical batch 32 768 → 65 536".
pub(crate) fn changes_line(changes: &[FitChange]) -> String {
    let n = |v: Option<u64>| v.map(crate::fmt::grouped).unwrap_or_else(|| "unset".into());
    changes
        .iter()
        .map(|c| {
            let what = match c.key.as_str() {
                "ctx_size" => "Context size",
                "ubatch_size" => "Physical batch",
                "batch_size" => "Batch size",
                _ => "Another setting",
            };
            format!("{what} {} → {}", n(c.from), n(c.to))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The note and "Fit again" for the saved row `model_id` of `target`
/// (`chat` or `aux`). `note` is the row's read's; `on_saved` gets the
/// changes a save wrote.
#[component]
pub fn FitAgainNote(
    target: &'static str,
    #[prop(into)] model_id: Signal<String>,
    #[prop(into)] note: Signal<Option<FitNote>>,
    on_saved: Callback<Vec<FitChange>>,
) -> impl IntoView {
    let toasts = use_toasts();
    let offer = RwSignal::new(None::<ModelFitAgain>);
    let busy = RwSignal::new(false);
    // What a save here answered, until the row is read again.
    let answered = RwSignal::new(None::<Option<FitNote>>);
    Effect::new(move |_| {
        note.with(|_| ());
        answered.set(None);
    });
    let shown = move || answered.get().unwrap_or_else(|| note.get());
    let ask = move |_| {
        busy.set(true);
        let mid = model_id.get_untracked();
        spawn_local(async move {
            match post(target, &mid, None).await {
                Ok(a) => offer.set(Some(a)),
                Err(e) => toasts.err(format!("Fit again: {e}")),
            }
            busy.set(false);
        });
    };
    let save = move |_| {
        let Some(changes) = offer.get_untracked().map(|o| o.changes) else {
            return;
        };
        busy.set(true);
        let mid = model_id.get_untracked();
        spawn_local(async move {
            match post(target, &mid, Some(changes)).await {
                Ok(a) if a.saved => {
                    toasts.ok(a.sentence.clone());
                    offer.set(None);
                    answered.set(Some(a.note.clone()));
                    on_saved.run(a.changes);
                }
                Ok(a) => offer.set(Some(a)),
                Err(e) => toasts.err(format!("Fit again: {e}")),
            }
            busy.set(false);
        });
    };
    let can_save = move || offer.with(|o| o.as_ref().is_some_and(|o| !o.changes.is_empty()));
    view! {
        {move || {
            let note = shown()?;
            Some(view! {
                <section class="card edit-section editor-top">
                    <h3>"Context fitted to the graphics card"</h3>
                    <p class="dim mini-note">{note_line(&note)}</p>
                    {move || {
                        offer.get().map(|o| {
                            let changes = (!o.changes.is_empty()).then(|| changes_line(&o.changes));
                            view! {
                                <p class="mini-note">{o.sentence.clone()}</p>
                                {changes.map(|c| view! { <p class="dim mini-note mono-sm">{c}</p> })}
                            }
                        })
                    }}
                    <div class="row" style="gap:8px">
                        <button
                            class="btn sm"
                            disabled=move || busy.get()
                            title="Take the fit on the graphics card as it is now; nothing is saved before you confirm"
                            on:click=ask
                        >
                            "Fit again"
                        </button>
                        <Show when=can_save>
                            <button class="btn sm primary" disabled=move || busy.get() on:click=save>
                                "Save"
                            </button>
                            <button class="btn sm" on:click=move |_| offer.set(None)>
                                "Cancel"
                            </button>
                        </Show>
                    </div>
                </section>
            })
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_reads_as_the_tokens_the_measure_and_when() {
        let n = FitNote {
            ctx: 32_768,
            trained_ctx: Some(262_144),
            measure: FitMeasure::Beside,
            capacity_bytes: 8 * 1024 * 1024 * 1024,
            headroom_bytes: 0,
            at: "2026-11-04 09:12:33".into(),
        };
        let line = note_line(&n);
        assert!(line.starts_with("Remembers 32"), "{line}");
        assert!(line.contains("beside what ran"), "{line}");
        assert!(line.contains("2026-11-04 09:12 UTC"), "{line}");
    }

    #[test]
    fn changes_read_by_their_labels() {
        let line = changes_line(&[
            FitChange {
                key: "ctx_size".into(),
                from: Some(8192),
                to: Some(4096),
            },
            FitChange {
                key: "ubatch_size".into(),
                from: Some(8192),
                to: Some(4096),
            },
        ]);
        assert!(
            line.starts_with("Context size ") && line.contains(" · Physical batch "),
            "{line}"
        );
    }
}
