//! The list pane (modern-sample.html): a header with the title, how many
//! builds and how many run, and the page's actions (New build, the menu with
//! Check now and Read again); the search; the builds as two-line rows under
//! their heading (the name, the state of the live or the last run, the
//! engine, the ref and the extras), then the images: every local image, and
//! where lmgw runs its engines natively, the pinned release builds.
//!
//! Each row is a link, so a click is the router's and a middle-click opens a
//! tab; the list keeps one tab stop. The header, search and keys are the
//! shared list pane's ([`crate::widgets::list_pane`]).

use std::collections::HashMap;

use leptos::prelude::*;
use lmgw_api_types::builds::{BuildRunStatus, BuildView};

use super::route::Sel;
use super::{engine_label, extra_short, phase_label, use_bk, Bk};
use crate::widgets::list_pane::{
    GroupHead, ListBox, ListHead, ListNav, ListNotice, ListRefs, ListSearch,
};
use crate::widgets::{filter_words, Dot, Ico, Icon, ListRow, MenuItem, RowMenu};

/// What a build is called on the page: its name, else its slug.
pub fn display_name(v: &BuildView) -> String {
    if v.build.spec.name.is_empty() {
        v.build.spec.slug.clone()
    } else {
        v.build.spec.name.clone()
    }
}

/// A build's dot and word: the live run's phase while one runs, else how
/// its last run ended, else that it never ran.
pub fn build_state(v: &BuildView, live: Option<&super::LiveRun>) -> (Dot, String) {
    if let Some(l) = live {
        let word = match (l.detail.phase, l.percent) {
            (lmgw_api_types::builds::BuildPhase::Waiting, _) => "waiting".to_string(),
            (p, Some(pct)) => format!("{} {pct}%", phase_label(p)),
            (p, None) => phase_label(p).to_string(),
        };
        return (Dot::Busy, word);
    }
    let Some(r) = v.last_run.as_ref() else {
        return (Dot::Stopped, "never run".to_string());
    };
    let dot = match r.status {
        BuildRunStatus::Running => Dot::Busy,
        BuildRunStatus::Succeeded | BuildRunStatus::UpToDate => Dot::Running,
        BuildRunStatus::Unverified => Dot::Warn,
        BuildRunStatus::Broken | BuildRunStatus::Failed => Dot::Error,
        BuildRunStatus::Canceled => Dot::Off,
    };
    (dot, super::status_label(r.status).to_string())
}

/// The row's second line: the engine, the ref, the extras merged on top,
/// and whether newer inputs wait.
pub fn build_meta(v: &BuildView) -> String {
    let s = &v.build.spec;
    let mut parts = vec![engine_label(s.engine).to_string(), s.git_ref.clone()];
    match s.extras.len() {
        0 => {}
        1 => parts.push(format!("+ {}", extra_short(&s.extras[0]))),
        n => parts.push(format!("+ {n} extras")),
    }
    if v.update.as_ref().is_some_and(|u| u.has_update()) {
        parts.push("update".to_string());
    }
    parts.join(" · ")
}

fn matches(v: &BuildView, words: &[String]) -> bool {
    let s = &v.build.spec;
    let hay = format!(
        "{} {} {} {} {} {}",
        s.name,
        s.slug,
        s.repo_url,
        s.git_ref,
        engine_label(s.engine),
        s.extras
            .iter()
            .map(|e| e.label())
            .collect::<Vec<_>>()
            .join(" ")
    )
    .to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

#[component]
pub fn ListPane(
    sel: Memo<Option<Sel>>,
    narrow: Memo<bool>,
    refs: ListRefs<Sel>,
    query: RwSignal<String>,
    /// Open an entry: it, and whether the move replaces the history entry.
    open: Callback<(Sel, bool)>,
) -> impl IntoView {
    let bk = use_bk();
    let bus = crate::live::use_live();
    let native = Memo::new(move |_| bk.releases.with(|r| r.as_ref().is_some_and(|r| r.native)));

    // The builds the search leaves, in the server's order.
    let shown = Memo::new(move |_| {
        let words = filter_words(&query.get());
        bk.builds.with(|b| {
            b.as_ref()
                .map(|b| {
                    b.iter()
                        .filter(|v| matches(v, &words))
                        .map(|v| v.build.id)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let order = Memo::new(move |_| {
        let mut o: Vec<Sel> = shown.get().into_iter().map(Sel::Build).collect();
        o.push(Sel::Images);
        if native.get() {
            o.push(Sel::Windows);
        }
        o
    });

    let nav = ListNav::new(refs, order, sel, narrow, open, |s: &Sel| s.path());

    let sub = move || {
        let Some(n) = bk.builds.with(|b| b.as_ref().map(Vec::len)) else {
            return "container images built from git".to_string();
        };
        let running = bk.live.with(HashMap::len);
        let builds = crate::fmt::count_of(n, "builds");
        if running > 0 {
            format!("{builds} · {running} running")
        } else {
            builds
        }
    };
    let more = page_menu(bk, bus);
    let new_build = move |_| {
        bk.ensure_env();
        bk.editor.set(Some(super::EditorSeed::blank()));
    };

    // A click highlights the row and leaves the focus on the list, where
    // the arrow keys go on from it (not on the link, which would draw its
    // own ring).
    let pick = move |s: Sel| {
        nav.cursor.set(Some(s));
        nav.focus_list();
    };
    // A row's address keeps the list's search, as the arrow keys do.
    let href_of = move |s: Sel| {
        move || {
            let q = query.get();
            if q.trim().is_empty() {
                s.path()
            } else {
                format!("{}?q={}", s.path(), js_sys::encode_uri_component(&q))
            }
        }
    };
    let build_row = move |id: i64| {
        let v = Memo::new(move |prev: Option<&BuildView>| {
            bk.builds
                .with(|b| {
                    b.as_ref()
                        .and_then(|b| b.iter().find(|x| x.build.id == id).cloned())
                })
                .or_else(|| prev.cloned())
                .unwrap_or_default()
        });
        let st = Memo::new(move |_| {
            let live = bk.live.with(|m| m.get(&id).cloned());
            v.with(|v| build_state(v, live.as_ref()))
        });
        let s = Sel::Build(id);
        let title = move || {
            v.with(|v| {
                format!(
                    "{}\n{}\n{}",
                    display_name(v),
                    v.build.spec.repo_url,
                    v.moving_tag
                )
            })
        };
        view! {
            <a
                class="lrow-link"
                href=href_of(s)
                tabindex="-1"
                title=title
                on:click=move |_| pick(s)
            >
                <ListRow
                    key=s.path()
                    name=move || v.with(display_name)
                    meta=move || v.with(build_meta)
                    dot=Signal::derive(move || st.get().0)
                    state=move || st.get().1
                    selected=Signal::derive(move || nav.lit(&s))
                />
            </a>
        }
    };

    let images_state = move || {
        let pulling = bk.pulls.with(HashMap::len);
        if pulling > 0 {
            return (Dot::Busy, format!("{pulling} pulling"));
        }
        let upd = bus
            .updates
            .with(|u| u.as_ref().map_or(0, |u| u.images_with_updates));
        if upd > 0 {
            return (
                Dot::Warn,
                format!("{upd} update{}", if upd == 1 { "" } else { "s" }),
            );
        }
        (Dot::Off, String::new())
    };
    let images_meta = move || {
        bk.images.with(|r| match r {
            Some(r) => format!(
                "{} · {} on disk",
                crate::fmt::count_of(r.images.len(), "images"),
                crate::fmt::fmt_bytes(r.disk.images_total)
            ),
            None => "llama.cpp, audio.cpp and sd.cpp images on this machine".to_string(),
        })
    };
    let windows_state = move || {
        let n = bus
            .updates
            .with(|u| u.as_ref().map_or(0, |u| u.engines_with_updates));
        if n > 0 {
            (
                Dot::Warn,
                format!("{n} upstream release{}", if n == 1 { "" } else { "s" }),
            )
        } else {
            (Dot::Off, String::new())
        }
    };
    // An entry with nothing to say (no pull, no update) has no dot.
    let entry_row = move |sel: Sel,
                          icon: Ico,
                          name: &'static str,
                          meta: Signal<String>,
                          st: Signal<(Dot, String)>| {
        let has_state = Memo::new(move |_| st.with(|(_, w)| !w.is_empty()));
        view! {
            <a class="lrow-link" href=href_of(sel) tabindex="-1" on:click=move |_| pick(sel)>
                {move || {
                    if has_state.get() {
                        view! {
                            <ListRow
                                key=sel.path()
                                icon=icon
                                name=name
                                meta=move || meta.get()
                                dot=Signal::derive(move || st.get().0)
                                state=move || st.get().1
                                selected=Signal::derive(move || nav.lit(&sel))
                            />
                        }
                            .into_any()
                    } else {
                        view! {
                            <ListRow
                                key=sel.path()
                                icon=icon
                                name=name
                                meta=move || meta.get()
                                selected=Signal::derive(move || nav.lit(&sel))
                            />
                        }
                            .into_any()
                    }
                }}
            </a>
        }
    };
    let loaded = move || bk.builds.with(Option::is_some);
    let actions = ViewFn::from(move || {
        view! {
            <button
                type="button"
                class="icon-btn"
                title="New build"
                aria-label="New build"
                on:click=new_build
            >
                <Icon ico=Ico::Plus/>
            </button>
            <RowMenu items=more icon=Ico::More title="More"/>
        }
    });

    view! {
        <ListHead scrolled=nav.scrolled narrow=narrow title="Engines" sub=Signal::derive(sub) actions=actions/>
        <ListSearch
            nav=nav
            query=query
            placeholder="Search builds"
            title="Search by name, repository, ref, engine or extra"
        />
        <ListNotice
            error=bk.builds_err
            stale=Signal::derive(move || bk.builds.with(Option::is_some))
            failed="Loading the builds failed: "
            refresh_failed="Refreshing the builds failed: "
            retry=Callback::new(move |()| bk.load_builds())
        />
        <ListBox nav=nav label="Builds and images" class="bk-list">
            <GroupHead label="Builds" n=Signal::derive(move || shown.with(Vec::len))/>
            <Show when=move || !loaded() && bk.builds_err.with(Option::is_none)>
                <div class="mlist-empty-class">"Loading…"</div>
            </Show>
            <Show when=move || loaded() && shown.with(Vec::is_empty)>
                <div class="mlist-empty-class">
                    {move || {
                        let q = query.get();
                        if q.trim().is_empty() {
                            "No builds yet: start one with +, or from a repository on the right.".to_string()
                        } else {
                            format!("No build matches \u{201c}{}\u{201d}", q.trim())
                        }
                    }}
                </div>
            </Show>
            // Keyed by the build: a re-read list updates the row in place.
            <For each=move || shown.get() key=|id| *id let:id>
                {build_row(id)}
            </For>
            <GroupHead label="Images"/>
            {entry_row(Sel::Images, Ico::Layers, "Local images", Signal::derive(images_meta), Signal::derive(images_state))}
            <Show when=move || native.get()>
                {entry_row(
                    Sel::Windows,
                    Ico::Download,
                    "Release builds",
                    Signal::stored("the pinned engine releases lmgw runs on Windows".to_string()),
                    Signal::derive(windows_state),
                )}
            </Show>
        </ListBox>
    }
}

/// The page's menu: Check now (every build, every registry image in use,
/// and on Windows the engine releases) and Read again.
fn page_menu(bk: Bk, bus: crate::live::LiveBus) -> Signal<Vec<MenuItem>> {
    Signal::derive(move || {
        let checking = bk.checking.with(|c| c.contains(&0));
        let last = bus
            .updates
            .with(|u| u.as_ref().and_then(|u| u.checked_at.clone()))
            .map(|at| {
                format!(
                    "Last full check {} ({}).",
                    super::ago(&at),
                    super::local_ts(&at)
                )
            })
            .unwrap_or_else(|| "No full check has run yet.".to_string());
        vec![
            MenuItem::new(
                if checking {
                    "Checking for updates…"
                } else {
                    "Check for updates now"
                },
                move || bk.check_updates(None, "update check".to_string()),
            )
            .disabled(checking)
            .title(format!(
                "Ask the remotes now: every build's ref and unpinned extras, the registry images in use, and on Windows the pinned engine releases' upstream projects. {last} The schedule is in Settings → Engines."
            )),
            MenuItem::new("Read again", move || {
                bk.load_builds();
                if bk.images_wanted.get_untracked() {
                    bk.load_images();
                }
                bk.load_releases();
            })
            .title("Read the builds, the local images and the release builds again"),
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::builds::{BuildRun, BuildSpec, Engine};

    fn view_of(name: &str, git_ref: &str) -> BuildView {
        BuildView {
            build: lmgw_api_types::builds::Build {
                spec: BuildSpec {
                    name: name.into(),
                    slug: "demo-slug".into(),
                    git_ref: git_ref.into(),
                    engine: Engine::Llama,
                    repo_url: "https://git.example/llama.cpp".into(),
                    ..BuildSpec::default()
                },
                ..Default::default()
            },
            ..BuildView::default()
        }
    }

    #[test]
    fn a_build_never_run_says_so_and_a_failed_one_is_red() {
        let mut v = view_of("demo", "master");
        assert_eq!(build_state(&v, None), (Dot::Stopped, "never run".into()));
        v.last_run = Some(BuildRun {
            status: BuildRunStatus::Failed,
            ..BuildRun::default()
        });
        assert_eq!(build_state(&v, None), (Dot::Error, "failed".into()));
        v.last_run = Some(BuildRun {
            status: BuildRunStatus::Succeeded,
            ..BuildRun::default()
        });
        assert_eq!(build_state(&v, None).0, Dot::Running);
    }

    #[test]
    fn the_search_reads_name_slug_repository_and_ref() {
        let v = view_of("Demo build", "b1234");
        assert!(matches(&v, &["demo".into(), "b1234".into()]));
        assert!(matches(&v, &["example".into()]));
        assert!(matches(&v, &["demo-slug".into()]));
        assert!(!matches(&v, &["vulkan-only".into()]));
    }

    #[test]
    fn the_second_line_names_engine_ref_and_extras() {
        let v = view_of("demo", "master");
        assert_eq!(build_meta(&v), "llama.cpp · master");
    }
}
