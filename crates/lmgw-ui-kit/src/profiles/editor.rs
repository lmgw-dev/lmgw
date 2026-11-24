//! [`ProfileEditor`]: one profile, or a new one.

use leptos::prelude::*;
use lmgw_api_types::chat_profiles::{field, Profile, ProfileDeleted};

use wasm_bindgen::JsCast;

use super::api;
use super::fields::{BehaviourGroup, ExamplesGroup, State, TextGroup, VoiceBlockGroup};
use super::model::{self, Form};
use super::try_panel::{SpeakPanel, StaticPart, TestPanel};
use crate::scope::Scope;
use crate::widgets::{ActionRow, EntryRow, Ico, Icon, Modal, ModalFooter, PrefGroup, SaveToast};

/// Edit one personality profile, or write a new one (`profile` is `None`),
/// as preference groups with the save toast.
///
/// The component holds its own state: mount it again (key it on the
/// profile's id) to edit another. It needs the kit's contexts as every
/// picker does (the model catalog) and an `<audio>`-capable webview for
/// Speak; it needs no toast host. The save toast is `position: absolute`
/// at the bottom centre (kit.css, "save toast"): the host gives the editor
/// a positioned ancestor to hold it, which should not scroll with the
/// editor's content.
///
/// - `thread_id`: preview, test and speak assemble as that thread would
///   (its prompt, languages and voice overrides) with the draft in place of
///   its own profile; absent, an empty thread with Settings' defaults.
/// - `test_model`: the alias Test and Count start on (the thread's, when
///   opened from a thread).
/// - `is_default`: the profile is the one new threads start with, so the
///   delete question says it clears that too.
/// - `hero`: the editor starts with a hero: the profile's glyph, name, the
///   first line of its persona and chips for built-in, usage and default.
/// - `on_saved`: the stored profile after a create or a save, and after a
///   reset when `on_reset` is absent.
/// - `on_reset`: the stored profile after a reset to the built-in, so the
///   host can say which of save, reset and delete happened.
/// - `on_deleted`: what the delete cleared.
/// - `on_discard_new`: Discard on a new profile that has edits: the host
///   drops the draft, unmounting the editor. Without it Discard empties the
///   form, as on a stored row it restores what was stored.
/// - `dirty`: kept `true` while there are unsaved edits, for a host that
///   guards leaving.
/// - `install_save`: called once, while the editor is built, with its save,
///   for a host that binds it to a key (the dashboard's Ctrl+S). Without it
///   the editor binds Ctrl+S itself while it is mounted.
/// - `install_toast`: the save toast is not drawn in the editor but handed to
///   the host (called with it while the editor is built, and with `None`
///   when it goes), which draws it beside the scroller the editor is in: a
///   scroller that is a container query container is the containing block of
///   anything absolute in it, so a toast drawn inside would scroll with the
///   groups. Without it the toast is drawn in the editor, which keeps room
///   under its last group so the toast never covers Reset and Delete.
///
/// A reset or a delete the gateway refuses shows as a dismissible message
/// under those rows, not in the save toast.
#[component]
pub fn ProfileEditor(
    profile: Option<Profile>,
    #[prop(default = None)] thread_id: Option<i64>,
    #[prop(default = None, into)] test_model: Option<String>,
    #[prop(default = false)] is_default: bool,
    #[prop(optional)] on_saved: Option<Callback<Profile>>,
    #[prop(optional)] on_reset: Option<Callback<Profile>>,
    #[prop(optional)] on_deleted: Option<Callback<ProfileDeleted>>,
    #[prop(optional)] on_discard_new: Option<Callback<()>>,
    #[prop(optional)] dirty: Option<RwSignal<bool>>,
    #[prop(default = false)] hero: bool,
    #[prop(optional)] install_save: Option<Callback<Callback<()>>>,
    #[prop(optional)] install_toast: Option<Callback<Option<ViewFn>>>,
) -> impl IntoView {
    let http = crate::http::use_http();
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
    // What blocks a save: each says so on its own row. The examples' text
    // form that does not read counts too.
    let problems = Memo::new(move |_| {
        form.get().problems().len() + usize::from(st.ex_err.with(Option::is_some))
    });
    if let Some(d) = dirty {
        Effect::new(move |_| d.set(!changed.get().is_empty()));
    }

    let saving = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // A refused reset or delete: its own message, not a failed save.
    let action_err = RwSignal::new(None::<String>);
    let scope = Scope::new();

    let applied = move |p: Profile, was_reset: bool| {
        base.set(Form::of(Some(&p)));
        st.load(&base.get_untracked());
        stored.set(Some(p.clone()));
        error.set(None);
        action_err.set(None);
        if let Some(cb) = on_reset.filter(|_| was_reset).or(on_saved) {
            cb.run(p);
        }
    };
    let save = move |()| {
        if saving.get_untracked()
            || problems.get_untracked() > 0
            || changed.with_untracked(Vec::is_empty)
        {
            return;
        }
        let f = form.get_untracked();
        let b = base.get_untracked();
        let current = stored.get_untracked();
        saving.set(true);
        scope.spawn(async move {
            let res = match &current {
                None => api::create(http, &f.create()).await,
                Some(p) => api::patch(http, p.id, &f.patch(&b)).await,
            };
            saving.set(false);
            match res {
                Ok(p) => applied(p, false),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    let discard = move |()| {
        if let (None, Some(cb)) = (stored.get_untracked(), on_discard_new) {
            cb.run(());
            return;
        }
        st.load(&base.get_untracked());
        error.set(None);
    };
    let reset = move |()| {
        let Some(p) = stored.get_untracked() else {
            return;
        };
        action_err.set(None);
        saving.set(true);
        scope.spawn(async move {
            let res = api::reset(http, p.id).await;
            saving.set(false);
            match res {
                Ok(p) => applied(p, true),
                Err(e) => action_err.set(Some(format!("Not reset: {e}"))),
            }
        });
    };
    let delete = Callback::new(move |()| {
        let Some(p) = stored.get_untracked() else {
            return;
        };
        action_err.set(None);
        scope.spawn(async move {
            match api::delete(http, p.id).await {
                Ok(d) => {
                    if let Some(cb) = on_deleted {
                        cb.run(d);
                    }
                }
                Err(e) => action_err.set(Some(format!("Not deleted: {e}"))),
            }
        });
    });

    let builtin = move || stored.get().and_then(|p| p.builtin);
    let all_follow = move || {
        stored
            .get()
            .is_some_and(|p| p.follows_builtin.len() >= field::FOLLOWABLE.len())
    };
    let invalid = Signal::derive(move || {
        // A fresh form is not scolded; once there is something to save (or
        // the examples' text does not read) each problem counts.
        if changed.with(|c| !c.is_empty()) || st.ex_err.with(Option::is_some) {
            problems.get()
        } else {
            0
        }
    });
    let name_error = Signal::derive(move || {
        // Only once typed in, or on a stored row: a fresh form is not scolded.
        let n = st.name.get();
        (stored.get().is_some() || !n.is_empty())
            .then(|| model::name_problem(&n))
            .flatten()
    });
    let save_cb = Callback::new(save);
    match install_save {
        Some(install) => install.run(save_cb),
        None => {
            let keys = window_event_listener(leptos::ev::keydown, move |ev| {
                if !(ev.ctrl_key() || ev.meta_key())
                    || ev.alt_key()
                    || ev.shift_key()
                    || !ev.key().eq_ignore_ascii_case("s")
                    || matches!(document().query_selector("dialog[open]"), Ok(Some(_)))
                {
                    return;
                }
                ev.prevent_default();
                // The field being typed in commits first (its `change`).
                let active = document()
                    .active_element()
                    .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok());
                if let Some(a) = &active {
                    let _ = a.blur();
                }
                save_cb.run(());
                if let Some(a) = active {
                    let _ = a.focus();
                }
            });
            on_cleanup(move || keys.remove());
        }
    }
    let toast = ViewFn::from(move || {
        view! {
            <SaveToast
                dirty_count=Signal::derive(move || changed.with(Vec::len))
                invalid=invalid
                saving=saving
                error=error
                on_save=save_cb
                on_discard=Callback::new(discard)
                save_label=move || if stored.get().is_some() { "Save" } else { "Create profile" }
            />
        }
    });
    let toast_room = install_toast.is_none();
    let toast_inline = match install_toast {
        Some(install) => {
            install.run(Some(toast));
            on_cleanup(move || install.run(None));
            None
        }
        None => Some(toast.run()),
    };
    let asking = RwSignal::new(false);
    let delete_sentence = move || {
        stored
            .get()
            .map(|p| model::delete_question(&p.used_by, is_default, p.builtin.is_some()))
            .unwrap_or_default()
    };

    view! {
        <div class="pf-editor" class:pf-toast-room=toast_room>
            {hero.then(|| view! { <Hero st=st stored=stored is_default=is_default/> })}
            <PrefGroup title="Profile">
                <EntryRow
                    title="Name"
                    value=st.name
                    placeholder="For example Concise"
                    error=name_error
                    dirty=Signal::derive(move || changed.with(|c| c.contains(&"name")))
                />
            </PrefGroup>
            <TextGroup st=st changed=changed/>
            <ExamplesGroup st=st changed=changed/>
            <VoiceBlockGroup st=st changed=changed reference=reference.into()/>
            <BehaviourGroup st=st changed=changed base=base list_id=list_id/>
            <StaticPart form=form thread_id=thread_id reference=reference count_model=count_model/>
            <TestPanel form=form thread_id=thread_id model=model last_reply=last_reply/>
            <SpeakPanel form=form thread_id=thread_id last_reply=last_reply/>
            <PrefGroup>
                {move || {
                    builtin()
                        .map(|_| {
                            let off = move || all_follow() || saving.get();
                            let title = "Persona, length rule, examples, voice block and reasoning take the built-in text again; name and voice stay. Unsaved edits are dropped.";
                            view! {
                                <ActionRow
                                    title="Reset to built-in"
                                    sub=title
                                    on_click=Callback::new(move |()| reset(()))
                                    disabled=Signal::derive(off)
                                />
                            }
                        })
                }}
                {move || {
                    stored
                        .get()
                        .map(|_| {
                            view! {
                                <ActionRow
                                    title="Delete profile…"
                                    sub="Removes it; what uses it goes back to none"
                                    danger=true
                                    on_click=Callback::new(move |()| asking.set(true))
                                />
                            }
                        })
                }}
            </PrefGroup>
            {move || {
                action_err
                    .get()
                    .map(|e| {
                        view! {
                            <div class="pf-action-err" role="alert">
                                <span>{e}</span>
                                <button
                                    type="button"
                                    class="icon-btn sm"
                                    title="Dismiss"
                                    aria-label="Dismiss"
                                    on:click=move |_| action_err.set(None)
                                >
                                    <Icon ico=Ico::Close/>
                                </button>
                            </div>
                        }
                    })
            }}
            {toast_inline}
            <Modal open=asking title="Delete this profile?">
                <p>{delete_sentence}</p>
                <ModalFooter>
                    <button type="button" class="btn ghost" on:click=move |_| asking.set(false)>
                        "Cancel"
                    </button>
                    <button
                        type="button"
                        class="btn danger"
                        on:click=move |_| {
                            asking.set(false);
                            delete.run(());
                        }
                    >
                        "Delete"
                    </button>
                </ModalFooter>
            </Modal>
        </div>
    }
}

/// The editor's hero: the profile's glyph and name, the first line of its
/// persona, and chips for what it is and who uses it.
#[component]
fn Hero(st: State, stored: RwSignal<Option<Profile>>, is_default: bool) -> impl IntoView {
    let title = move || {
        let name = stored
            .get()
            .map(|p| p.name)
            .unwrap_or_else(|| st.name.get());
        if name.trim().is_empty() {
            "New profile".to_string()
        } else {
            name
        }
    };
    let sub = move || {
        let persona = stored
            .get()
            .map(|p| p.persona)
            .unwrap_or_else(|| st.persona.get());
        let first = persona.lines().map(str::trim).find(|l| !l.is_empty());
        first.map_or_else(|| "No persona yet".to_string(), str::to_string)
    };
    view! {
        <div class="detail-hero">
            <div class="glyph">
                <Icon ico=Ico::Chat/>
            </div>
            <div class="hero-text">
                <h1>{title}</h1>
                <div class="sub pf-hero-sub" title=sub>{sub}</div>
                <div class="chips">
                    {move || {
                        stored
                            .get()
                            .and_then(|p| p.builtin)
                            .map(|_| {
                                view! {
                                    <span
                                        class="chip info"
                                        title="Ships with lmgw. Fields you leave alone follow its improvements."
                                    >
                                        "Built-in"
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
                                    (0, 0) => "Not used".to_string(),
                                    _ => format!(
                                        "Used by {} · {}",
                                        crate::fmt::count_of(n as usize, "threads"),
                                        crate::fmt::count_of(f, "folders"),
                                    ),
                                };
                                view! { <span class="chip">{text}</span> }
                            })
                    }}
                    {is_default.then(|| view! { <span class="chip">"Default for new threads"</span> })}
                </div>
            </div>
        </div>
    }
}
