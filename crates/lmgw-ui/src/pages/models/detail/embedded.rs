//! The editors of the aux, audio and image classes, the cloud aliases and
//! the candidate aliases, in the detail pane: their forms as the modals had
//! them (fields, checks, Save and Cancel at their foot), in a boxed group.
//! Porting them to preference rows is not done yet.
//!
//! A form is mounted with the row as it was loaded. Cancel puts it back to
//! that; a save that closes the form mounts it again once the list has the
//! saved row, and so does any reload of the list while nothing was typed
//! (a form that stays open after its save, the image form on its command
//! line, keeps it through the reload that save asked for). The forms reach
//! the pane through [`PaneEditor`] where they would reach their modal: a
//! save clears its unsaved state, an edit by a button counts as one, and
//! Close and Cancel put the form back. Leaving the row with unsaved input
//! asks first, and Ctrl+S presses the form's Save.

use leptos::html;
use leptos::prelude::*;

use super::super::route::Sel;
use super::super::split::SaveKey;
use super::super::{bump, Class, PageCtx};
use crate::pages::candidate_alias_editor::CandidateAliasForm;
use crate::pages::editor_host::PaneEditor;
use crate::pages::model_editors::{AliasForm, AudioForm, AuxForm, ImageForm};
use crate::widgets::use_touched;

#[component]
pub(super) fn EmbeddedForm(page: PageCtx, class: Class, id: i64) -> impl IntoView {
    let data = page.data;
    // Bumped to mount the form again, from the row as the list now has it.
    let generation = RwSignal::new(0u32);
    // A save closed the form, or the list had not arrived when it was
    // mounted: mount it again once the list has (re)loaded.
    let after_reload = StoredValue::new(false);
    // A save of this form asked the list to reload, and it has not landed.
    let reload_pending = StoredValue::new(false);
    // Whether the mounted form has unsaved input. An untouched form is
    // mounted again on every reload of the list too: a row's Enable or
    // Disable from the menu (or any other op) must not leave it showing the
    // row as it was, for a later Save to write back. The reload its own
    // save asked for leaves an open form be: it shows what the save said.
    let touched_now = StoredValue::new(None::<RwSignal<bool>>);
    Effect::new(move |prev: Option<()>| {
        data.full.track();
        if prev.is_none() {
            return;
        }
        let own_save = reload_pending.get_value();
        reload_pending.set_value(false);
        let untouched = touched_now
            .get_value()
            .is_some_and(|t| !t.try_get_untracked().unwrap_or(true));
        if after_reload.get_value() || (untouched && !own_save) {
            after_reload.set_value(false);
            generation.update(|g| *g += 1);
        }
    });
    move || {
        generation.track();
        let open = RwSignal::new(true);
        // Closed (Cancel, or Close after a save): back to the row as the
        // list has it, at once, or once the list has what a save changed.
        Effect::new(move |_| {
            if !open.get() {
                if reload_pending.get_value() {
                    after_reload.set_value(true);
                } else {
                    generation.update(|g| *g += 1);
                }
            }
        });
        let on_saved = move || {
            reload_pending.set_value(true);
            bump(page.reload);
        };
        let node: NodeRef<html::Div> = NodeRef::new();
        let touched = use_touched(node);
        touched_now.set_value(Some(touched));
        PaneEditor { touched, open }.provide();
        page.nav.guard().watch_in(
            "this model",
            Signal::derive(move || touched.get() && open.get()),
            Signal::stored(Sel::Row(class, id).path()),
        );
        SaveKey::install(Callback::new(move |()| {
            let Some(el) = node.get_untracked() else {
                return;
            };
            if let Ok(Some(b)) = el.query_selector(".modal-foot .btn.primary") {
                if let Ok(b) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(b) {
                    b.click();
                }
            }
        }));
        let form = data.full.with_untracked(|f| {
            let f = f.as_ref()?;
            Some(match class {
                Class::Aux => {
                    let m = f.aux.iter().find(|v| v.model.id == id)?.model.clone();
                    view! { <AuxForm m=m open=open on_saved=on_saved/> }.into_any()
                }
                Class::Audio => {
                    let m = f.audio.iter().find(|v| v.model.id == id)?.model.clone();
                    view! { <AudioForm m=m open=open on_saved=on_saved/> }.into_any()
                }
                Class::Image => {
                    let v = f.image.iter().find(|v| v.model.id == id)?;
                    page.editors.image_derived.set(v.derived_prompt_style);
                    let m = v.model.clone();
                    view! { <ImageForm m=m open=open on_saved=on_saved/> }.into_any()
                }
                Class::Alias => {
                    let a = f.aliases.iter().find(|a| a.id == id)?.clone();
                    view! { <AliasForm a=a open=open on_saved=on_saved/> }.into_any()
                }
                Class::Candidate => {
                    let c = f.candidate_aliases.iter().find(|c| c.id == id)?.clone();
                    view! { <CandidateAliasForm c=c full=data.full open=open on_saved=on_saved/> }
                        .into_any()
                }
                Class::Chat => return None,
            })
        });
        if form.is_none() {
            after_reload.set_value(true);
        }
        let title = match class {
            Class::Alias => "Alias",
            Class::Candidate => "Candidate alias",
            _ => "Settings",
        };
        view! {
            <section class="pref-group embedded">
                <div class="pref-head">
                    <h2>{title}</h2>
                </div>
                <div class="boxed embedded-form" node_ref=node>
                    {form.unwrap_or_else(|| view! { <div class="prow dim">"Loading…"</div> }.into_any())}
                </div>
            </section>
        }
    }
}
