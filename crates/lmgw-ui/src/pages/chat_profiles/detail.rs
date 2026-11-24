//! The detail pane: nothing selected (what the list holds, and the way to
//! add one), a profile or a new one (the kit's editor under the hero and
//! the header bar), or a selection the gateway does not have.

use leptos::prelude::*;
use lmgw_api_types::chat_profiles::{Profile, ProfileDeleted, BUILTIN_CONCISE};
use lmgw_ui_kit::profiles::ProfileEditor;

use super::{Page, Sel};
use crate::widgets::nav_split::{track_scroll, DetailHead, SaveKey};
use crate::widgets::{ActionRow, Ico, Icon, InfoRow, PrefGroup};

#[component]
pub(super) fn DetailPane(
    page: Page,
    sel: Memo<Option<Sel>>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    // Whether the selected profile is (still) there; not known yet counts
    // as there, so the page does not flash "gone".
    let known = Memo::new(move |_| match sel.get() {
        Some(Sel::Row(id)) => page
            .list
            .with(|l| l.as_ref().map(|l| l.profiles.iter().any(|p| p.id == id))),
        _ => Some(true),
    });
    let present = Memo::new(move |_| known.get() != Some(false));
    move || {
        let s = sel.get();
        if !present.get() {
            return view! { <Gone narrow=narrow back=back/> }.into_any();
        }
        match s {
            None => view! { <StatusPage page=page narrow=narrow back=back/> }.into_any(),
            Some(Sel::New) => {
                view! { <ProfileDetail page=page id=None narrow=narrow back=back/> }.into_any()
            }
            Some(Sel::Row(id)) => {
                view! { <ProfileDetail page=page id=Some(id) narrow=narrow back=back/> }.into_any()
            }
        }
    }
}

/// A count, or a dash while the list is not read.
fn dash_or(n: Option<u64>) -> String {
    n.map_or_else(|| "–".to_string(), crate::fmt::grouped)
}

/// What a delete cleared, in a sentence for the toast.
fn deleted_words(name: &str, d: &ProfileDeleted) -> String {
    let mut cleared = Vec::new();
    if d.threads_cleared > 0 {
        cleared.push(format!(
            "{} now use none",
            crate::fmt::count_of(d.threads_cleared as usize, "threads")
        ));
    }
    if !d.folders_cleared.is_empty() {
        let names: Vec<String> = d
            .folders_cleared
            .iter()
            .map(|f| format!("'{}'", f.name))
            .collect();
        cleared.push(format!("{} no longer name it", names.join(", ")));
    }
    if d.default_cleared {
        cleared.push("new threads start with none".to_string());
    }
    if cleared.is_empty() {
        format!("Deleted '{name}'")
    } else {
        format!("Deleted '{name}': {}", cleared.join(", "))
    }
}

/// One profile, or a new one: the header bar, and the kit's editor in the
/// scroller. The save toast comes after the scroller (the scroller is a
/// container query container, and so the containing block of anything
/// absolute inside it: a toast in the editor would scroll with the groups).
#[component]
fn ProfileDetail(
    page: Page,
    /// The saved profile; `None` makes a new one.
    id: Option<i64>,
    narrow: Memo<bool>,
    back: Callback<()>,
) -> impl IntoView {
    let row = Memo::new(move |_| id.and_then(|id| page.by_id(id)));
    // A deep link reads the list first: the editor shows once it has the
    // row (and the thread's model, which Test starts on).
    let loaded =
        Memo::new(move |_| page.ready.get() && (id.is_none() || row.with(Option::is_some)));
    let here = id.map_or(Sel::New, Sel::Row);
    let dirty = RwSignal::new(false);
    page.nav
        .guard()
        .watch_in("this profile", dirty.into(), Signal::stored(here.path()));
    let toast = RwSignal::new(None::<ViewFn>);
    let title = Signal::derive(move || {
        row.get()
            .map(|p| p.name)
            .unwrap_or_else(|| "New profile".to_string())
    });
    let scrolled = RwSignal::new(false);
    // Read where the editor is built and not tracked: the editor holds the
    // open profile's unsaved edits, so a list change must not build it again.
    let is_default = id.is_some_and(|id| {
        page.list
            .with_untracked(|l| l.as_ref().and_then(|l| l.default_profile_id))
            == Some(id)
    });
    let on_saved = Callback::new(move |p: Profile| {
        let (name, new_id) = (p.name.clone(), p.id);
        page.stored(p);
        if id.is_none() {
            page.toasts.ok(format!("Created '{name}'"));
            // The form is clean (the editor took the stored row): move to
            // its row without asking.
            page.nav.go_unasked(
                &format!(
                    "{}{}",
                    Sel::Row(new_id).path(),
                    page.nav.kept(super::LIST_KEYS)
                ),
                true,
            );
        } else {
            page.toasts.ok(format!("Saved '{name}'"));
        }
    });
    let on_reset = Callback::new(move |p: Profile| {
        let name = p.name.clone();
        page.stored(p);
        page.toasts.ok(format!("Reset '{name}'"));
    });
    let on_discard_new = Callback::new(move |()| {
        let before = page.before_new.get_value();
        let to = page.list.with_untracked(|l| match l {
            Some(l) => super::back_from_new(before, l),
            None => super::BASE.to_string(),
        });
        page.nav
            .go_unasked(&format!("{to}{}", page.nav.kept(super::LIST_KEYS)), true);
    });
    let on_deleted = Callback::new(move |d: ProfileDeleted| {
        let name = row.get_untracked().map(|p| p.name).unwrap_or_default();
        page.toasts.ok(deleted_words(&name, &d));
        page.load();
        page.nav.go_unasked(&page.base_href(), true);
    });
    view! {
        <DetailHead narrow=narrow back=back title=title scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow single">
                <Show when=move || loaded.get() fallback=|| view! { <div class="mdetail-empty">"Loading…"</div> }>
                    <ProfileEditor
                        profile=row.get_untracked()
                        thread_id=page.thread
                        test_model=page.test_model.get_untracked()
                        is_default=is_default
                        hero=true
                        dirty=dirty
                        on_saved=on_saved
                        on_reset=on_reset
                        on_deleted=on_deleted
                        on_discard_new=on_discard_new
                        install_save=Callback::new(SaveKey::install)
                        install_toast=Callback::new(move |t: Option<ViewFn>| toast.set(t))
                    />
                </Show>
            </div>
        </div>
        {move || toast.get().map(|t| t.run())}
    }
}

/// Nothing selected: what the list holds, and the way to add one.
#[component]
fn StatusPage(page: Page, narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    let count = move || {
        page.list
            .with(|l| l.as_ref().map(|l| l.profiles.len() as u64))
    };
    let default_name = move || {
        page.list.with(|l| {
            l.as_ref()
                .and_then(|l| {
                    let id = l.default_profile_id?;
                    l.profiles
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| p.name.clone())
                })
                .unwrap_or_else(|| if l.is_some() { "None" } else { "–" }.to_string())
        })
    };
    let threads = move || {
        page.list.with(|l| {
            l.as_ref()
                .map(|l| l.profiles.iter().map(|p| p.used_by.threads).sum::<i64>() as u64)
        })
    };
    let has_builtin = move || {
        page.list.with(|l| {
            l.as_ref().is_some_and(|l| {
                l.profiles
                    .iter()
                    .any(|p| p.builtin.as_deref() == Some(BUILTIN_CONCISE))
            })
        })
    };
    let loaded = move || page.list.with(Option::is_some);
    let settings = crate::pages::settings_href("chat_profile");
    view! {
        <DetailHead narrow=narrow back=back title="Chat profiles" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll" tabindex="-1" on:scroll=move |ev| track_scroll(&ev, scrolled)>
            <div class="detail-flow single">
                <div class="detail-hero">
                    <div class="glyph">
                        <Icon ico=Ico::Chat/>
                    </div>
                    <div class="hero-text">
                        <h1>"Chat profiles"</h1>
                        <div class="sub">
                            "How a chat's model talks, in text and in voice. Pick one in the list, or add one."
                        </div>
                    </div>
                </div>
                <PrefGroup title="In the list">
                    <InfoRow title="Profiles" value=move || dash_or(count())/>
                    <ActionRow
                        title="Default for new threads"
                        sub="Set in Settings, under Chat"
                        chevron=true
                        on_click=Callback::new(move |()| {
                            page.nav.go(&settings, false);
                        })
                    >
                        <span class="prow-value pf-nowrap">{default_name}</span>
                    </ActionRow>
                    <InfoRow title="Used by threads" value=move || dash_or(threads())/>
                </PrefGroup>
                <PrefGroup title="Add">
                    <ActionRow
                        title="New profile"
                        sub="Start from an empty one"
                        chevron=true
                        on_click=Callback::new(move |()| page.open_new(None))
                    />
                    <Show when=move || loaded() && !has_builtin()>
                        <ActionRow
                            title="Add the built-in Concise"
                            sub="The profile lmgw ships: short answers that suit voice"
                            chevron=true
                            on_click=Callback::new(move |()| page.add_builtin(true))
                        />
                    </Show>
                </PrefGroup>
                {page
                    .thread
                    .map(|t| {
                        let to = format!("/chat?t={t}");
                        view! {
                            <PrefGroup title="Opened from a thread">
                                <InfoRow
                                    title="Preview, Test and Speak"
                                    value=move || {
                                        match page.test_model.get() {
                                            Some(m) => format!("Assemble as thread {t} would, on its model {m}"),
                                            None => format!("Assemble as thread {t} would"),
                                        }
                                    }
                                />
                                <ActionRow
                                    title="Back to the thread"
                                    chevron=true
                                    on_click=Callback::new(move |()| {
                                        page.nav.go(&to, false);
                                    })
                                />
                            </PrefGroup>
                        }
                    })}
            </div>
        </div>
    }
}

/// A selection that names a profile the gateway does not have.
#[component]
fn Gone(narrow: Memo<bool>, back: Callback<()>) -> impl IntoView {
    let scrolled = RwSignal::new(false);
    view! {
        <DetailHead narrow=narrow back=back title="Not here" scrolled=scrolled/>
        <div class="detail-scroll mdetail-scroll">
            <div class="detail-flow single">
                <div class="mdetail-empty">
                    <p>"This profile is not in the gateway: it was deleted, or the link names one that never was."</p>
                    <button type="button" class="btn" on:click=move |_| back.run(())>
                        "Back to the list"
                    </button>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::chat_profiles::FolderRef;

    #[test]
    fn a_delete_says_what_it_cleared() {
        assert_eq!(
            deleted_words("Concise", &ProfileDeleted::default()),
            "Deleted 'Concise'"
        );
        let d = ProfileDeleted {
            deleted: 1,
            threads_cleared: 2,
            folders_cleared: vec![FolderRef {
                id: 4,
                name: "Assistant".into(),
            }],
            default_cleared: true,
        };
        assert_eq!(
            deleted_words("Concise", &d),
            "Deleted 'Concise': 2 threads now use none, 'Assistant' no longer name it, new threads start with none"
        );
    }
}
