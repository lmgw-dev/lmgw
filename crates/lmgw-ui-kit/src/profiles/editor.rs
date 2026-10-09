//! [`ProfileEditor`]: one profile, or a new one.

use leptos::prelude::*;
use lmgw_api_types::chat_profiles::{field, Profile, ProfileDeleted};

use super::api;
use super::fields::{BehaviourFields, ExamplesField, State, TextFields, VoiceBlockField};
use super::model::{self, Form};
use super::try_panel::{SpeakPanel, StaticPart, TestPanel};
use crate::scope::Scope;
use crate::widgets::confirm::ConfirmButton;
use crate::widgets::form::{Field, SaveBar};

/// Edit one personality profile, or write a new one (`profile` is `None`).
///
/// The component holds its own state: mount it again (key it on the
/// profile's id) to edit another. It needs the kit's contexts as every
/// picker does (the model catalog) and an `<audio>`-capable webview for
/// Speak; it needs no toast host.
///
/// - `thread_id`: preview, test and speak assemble as that thread would
///   (its prompt, languages and voice overrides) with the draft in place of
///   its own profile; absent, an empty thread with Settings' defaults.
/// - `test_model`: the alias Test and Count start on (the thread's, when
///   opened from a thread).
/// - `is_default`: the profile is the one new threads start with, so the
///   delete question says it clears that too.
/// - `on_saved`: the stored profile after a create, a save or a reset.
/// - `on_deleted`: what the delete cleared.
/// - `on_discard_new`: Discard on a new profile (pressable even before
///   anything is typed): the host drops the draft, unmounting the editor.
///   Without it Discard empties the form, as on a stored row it restores
///   what was stored.
/// - `dirty`: kept `true` while there are unsaved edits, for a host that
///   guards leaving.
#[component]
pub fn ProfileEditor(
    profile: Option<Profile>,
    #[prop(default = None)] thread_id: Option<i64>,
    #[prop(default = None, into)] test_model: Option<String>,
    #[prop(default = false)] is_default: bool,
    #[prop(optional)] on_saved: Option<Callback<Profile>>,
    #[prop(optional)] on_deleted: Option<Callback<ProfileDeleted>>,
    #[prop(optional)] on_discard_new: Option<Callback<()>>,
    #[prop(optional)] dirty: Option<RwSignal<bool>>,
) -> impl IntoView {
    let stored = RwSignal::new(profile);
    let base = RwSignal::new(Form::of(stored.get_untracked().as_ref()));
    let st = State::new(&base.get_untracked());
    let form = Signal::derive(move || st.form());
    let list_id = format!("pf-voices-{}", stored.get_untracked().map_or(0, |p| p.id));

    let reference = RwSignal::new(None::<String>);
    let model = RwSignal::new(test_model.unwrap_or_default());
    let count_model = RwSignal::new(model.get_untracked());
    let last_reply = RwSignal::new(None::<String>);

    let changed = Memo::new(move |_| {
        let f = form.get();
        // A new profile is dirty as soon as it has anything to save.
        base.with(|b| f.changed(b))
    });
    let problems = Memo::new(move |_| {
        let mut p = form.get().problems();
        if let Some(e) = st.ex_err.get() {
            p.push(format!("examples as text: {e}"));
        }
        p
    });
    if let Some(d) = dirty {
        Effect::new(move |_| d.set(!changed.get().is_empty()));
    }

    let saving = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let scope = Scope::new();

    let applied = move |p: Profile| {
        base.set(Form::of(Some(&p)));
        st.load(&base.get_untracked());
        stored.set(Some(p.clone()));
        error.set(None);
        if let Some(cb) = on_saved {
            cb.run(p);
        }
    };
    let save = move |_| {
        if saving.get_untracked() || !problems.get_untracked().is_empty() {
            return;
        }
        let f = form.get_untracked();
        let b = base.get_untracked();
        let current = stored.get_untracked();
        saving.set(true);
        scope.spawn(async move {
            let res = match &current {
                None => api::create(&f.create()).await,
                Some(p) => api::patch(p.id, &f.patch(&b)).await,
            };
            saving.set(false);
            match res {
                Ok(p) => applied(p),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    let discard = move |_| {
        if let (None, Some(cb)) = (stored.get_untracked(), on_discard_new) {
            cb.run(());
            return;
        }
        st.load(&base.get_untracked());
        error.set(None);
    };
    let reset = move |_| {
        let Some(p) = stored.get_untracked() else {
            return;
        };
        saving.set(true);
        scope.spawn(async move {
            let res = api::reset(p.id).await;
            saving.set(false);
            match res {
                Ok(p) => applied(p),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    let delete = Callback::new(move |_| {
        let Some(p) = stored.get_untracked() else {
            return;
        };
        scope.spawn(async move {
            match api::delete(p.id).await {
                Ok(d) => {
                    if let Some(cb) = on_deleted {
                        cb.run(d);
                    }
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    });

    let builtin = move || stored.get().and_then(|p| p.builtin);
    let all_follow = move || {
        stored
            .get()
            .is_some_and(|p| p.follows_builtin.len() >= field::FOLLOWABLE.len())
    };
    let detail = Signal::derive(move || changed.get().join(", "));
    let invalid = Signal::derive(move || problems.get().len());
    let name_error = Signal::derive(move || {
        // Only once typed in, or on a stored row: a fresh form is not scolded.
        let n = st.name.get();
        (stored.get().is_some() || !n.is_empty())
            .then(|| model::name_problem(&n))
            .flatten()
    });

    view! {
        <div class="pf-editor">
            <div class="pf-head">
                <Field label="Name" error=name_error dirty=Signal::derive(move || changed.get().contains(&"name"))>
                    <input
                        class="input"
                        spellcheck="false"
                        placeholder="e.g. Concise"
                        prop:value=move || st.name.get()
                        on:input=move |ev| st.name.set(event_target_value(&ev))
                    />
                </Field>
                <div class="pf-badges">
                    {move || {
                        builtin()
                            .map(|_| {
                                view! {
                                    <span
                                        class="chip info"
                                        title="Ships with lmgw. Fields you leave alone follow its improvements."
                                    >
                                        "built-in"
                                    </span>
                                }
                            })
                    }}
                    {move || {
                        stored
                            .get()
                            .map(|p| {
                                let n = p.used_by.threads;
                                let f = p.used_by.folders.len();
                                let text = match (n, f) {
                                    (0, 0) => "not used".to_string(),
                                    _ => format!(
                                        "{} · {}",
                                        crate::fmt::count_of(n as usize, "threads"),
                                        crate::fmt::count_of(f, "folders"),
                                    ),
                                };
                                view! { <span class="chip off">{text}</span> }
                            })
                    }}
                    {move || is_default.then(|| view! { <span class="chip off">"default for new threads"</span> })}
                </div>
            </div>

            <div class="field-grid pf-fields" style="--field-min:100%">
                <TextFields st=st />
                <ExamplesField st=st />
                <VoiceBlockField st=st reference=reference.into() />
                <BehaviourFields st=st list_id=list_id />
            </div>

            {move || {
                let p = problems.get();
                (!p.is_empty() && !changed.get().is_empty())
                    .then(|| view! { <ul class="pf-problems">{p.into_iter().map(|t| view! { <li>{t}</li> }).collect_view()}</ul> })
            }}

            <StaticPart form=form thread_id=thread_id reference=reference count_model=count_model />
            <TestPanel form=form thread_id=thread_id model=model last_reply=last_reply />
            <SpeakPanel form=form thread_id=thread_id last_reply=last_reply />

            <SaveBar
                dirty_count=Signal::derive(move || changed.get().len())
                detail=detail
                invalid=invalid
                saving=saving
                error=error
                on_save=Callback::new(save)
                on_discard=Callback::new(discard)
                save_label=Signal::derive(move || {
                    if stored.get().is_some() { "Save changes" } else { "Create profile" }
                })
                discard_clean=Signal::derive(move || on_discard_new.is_some() && stored.get().is_none())
            />
            <div class="pf-foot">
                {move || {
                    builtin()
                        .map(|_| {
                            view! {
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    disabled=move || all_follow() || saving.get()
                                    title="Persona, length rule, examples, voice block and reasoning take the built-in text again; name and voice stay. Unsaved edits are dropped."
                                    on:click=reset
                                >
                                    "Reset to built-in"
                                </button>
                            }
                        })
                }}
                {move || {
                    stored
                        .get()
                        .map(|p| {
                            let q = model::delete_question(&p.used_by, is_default, p.builtin.is_some());
                            view! {
                                <ConfirmButton
                                    label="Delete"
                                    confirm=q
                                    on_confirm=delete
                                    class="btn danger sm"
                                />
                            }
                        })
                }}
            </div>
        </div>
    }
}
