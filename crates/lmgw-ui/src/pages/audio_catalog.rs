//! "Add from catalog" — the audio twin of the HF wizard: browse audio.cpp's
//! own `model_specs` families, install a package's weights through the shared
//! download queue, then hand the finished package to the audio-model editor,
//! prefilled.
//!
//! Mirrors the pre-cutover Audio page exactly where it matters: the catalog is
//! a *cached* snapshot (nothing is fetched until "Refresh catalog" is
//! clicked), installing means queueing the package's files into the audio
//! models dir, and the model row is only ever created by the user confirming
//! the prefilled editor — family/task/voice config is not derivable from the
//! files alone.

mod view_state;

use std::collections::HashMap;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{AudioCatalog, AudioCatalogInstall, AudioFamily, AudioPackage, DownloadsView};
use serde_json::json;
use wasm_bindgen::JsCast;

use crate::fmt::hue_for;
use crate::widgets::use_toasts;
use view_state::{live_of, Landed, Live, Slot};

type CatalogResource = LocalResource<crate::api::Result<AudioCatalog>>;

/// What the list area shows. A Memo of its own, so the `.cat-list` (and the
/// keyed rows in it) is built only when this changes — a re-read of the
/// catalog lands in the rows that are already there.
#[derive(Clone, PartialEq)]
enum ListState {
    Loading,
    Failed(String),
    Empty,
    Ready,
}

/// The family ids of a read catalog, in the catalog's order; `None` while
/// nothing (or an error) has been read.
fn family_ids(c: &Option<crate::api::Result<AudioCatalog>>) -> Option<Vec<String>> {
    let c = c.as_ref()?.as_ref().ok()?;
    Some(c.families.iter().map(|f| f.family.clone()).collect())
}

/// One field of a row's current value; the default once the row has left
/// the catalog (it goes from the list with the next order update).
fn now<S, T: Default>(m: Memo<Option<S>>, pick: impl FnOnce(&S) -> T) -> T
where
    S: Send + Sync + 'static,
{
    m.with(|v| v.as_ref().map(pick).unwrap_or_default())
}

#[component]
pub fn AudioCatalogBrowser(open: RwSignal<bool>) -> impl IntoView {
    let toasts = use_toasts();
    let editors = expect_context::<super::models::Editors>();

    let reload = RwSignal::new(0u32);
    // The newest ask (`reload`'s count) a catalog read has answered. Set as
    // the read returns, before it is on screen: a landed download waits for
    // a read asked after it ([`Landed::asked`]), not just the next one in —
    // that may have left before the files did.
    let answered = StoredValue::new(0u32);
    let catalog = LocalResource::new(move || {
        let asked = reload.get();
        async move {
            let read = crate::api::get::<AudioCatalog>("/api/audio/catalog").await;
            answered.update_value(|a| *a = (*a).max(asked));
            read
        }
    });
    let dl_reload = RwSignal::new(0u32);
    let downloads = LocalResource::new(move || {
        dl_reload.get();
        crate::api::get::<DownloadsView>("/api/hf/downloads")
    });
    let refreshing = RwSignal::new(false);
    let refresh_err = RwSignal::new(None::<String>);
    // `(package id, download row id)` queued from here, so a package turns
    // amber before the first progress poll has come back.
    let watching = RwSignal::new(Vec::<(String, i64)>::new());
    // Rows that stopped moving and the catalog read that will count them:
    // their package shows "finishing" rather than offering a download of
    // files that are on disk.
    let landed = RwSignal::new(Vec::<Landed>::new());
    // What was moving at the last look ([`view_state::moving`]).
    let before = StoredValue::new(Vec::<(String, i64)>::new());

    // The families in the order the list shows them. The catalog sorts what is
    // here first, and it is re-read unasked (the poll, below, once a download
    // lands): following that order while the browser is open moved a family
    // out from under the pointer. So the order is seeded when the browser
    // opens and only merged into while it is open.
    let order = RwSignal::new(Vec::<String>::new());
    // Open/closed per family id, as the user left it. A family not in here
    // shows its default, decided once when its row is built. Lives as long as
    // the Models page does (the modal keeps its children), so a close and
    // reopen keeps it.
    let open_fams = RwSignal::new(HashMap::<String, bool>::new());
    let list_ref = NodeRef::<html::Div>::new();
    // Set when the browser opens: the next catalog read re-seeds the order
    // instead of merging into it. That read is the one the open asks for —
    // the catalog held at that moment is the one from before closing, and the
    // poll does not run while closed, so a download that landed meanwhile is
    // only in the fresh read.
    let reseed = StoredValue::new(false);

    Effect::new(move |_| {
        let Some(incoming) = catalog.with(family_ids) else {
            return;
        };
        let reseeding = reseed.get_value();
        reseed.set_value(false);
        let after = order.with_untracked(|o| view_state::after_read(o, &incoming, reseeding));
        if let Some(view_state::AfterRead {
            order: next,
            to_top,
        }) = after
        {
            order.set(next);
            // A new order on open: the list starts from the top rather than
            // at an offset into it. Next frame, once the rows are in it.
            if to_top {
                request_animation_frame(move || {
                    if let Some(el) = list_ref.get_untracked() {
                        el.set_scroll_top(0);
                    }
                });
            }
        }
    });

    // Reopening the browser re-reads both sides: a download may have finished
    // (or been started elsewhere) while it was closed. What was installed
    // since floats up with that read — on open, not later under the pointer.
    Effect::new(move |_| {
        if open.get() {
            reseed.set_value(true);
            reload.update(|n| *n += 1);
            dl_reload.update(|n| *n += 1);
        }
    });

    // What the last refresh could not do (a repo it could not list, a spec
    // file that did not load), shown until the next refresh replaces it.
    let warnings = Memo::new(move |_| {
        catalog.with(|c| {
            c.as_ref()
                .and_then(|r| r.as_ref().ok())
                .map(|c| c.warnings.clone())
                .unwrap_or_default()
        })
    });

    let list_state = Memo::new(move |_| {
        catalog.with(|c| match c {
            None => ListState::Loading,
            Some(Err(e)) => ListState::Failed(e.to_string()),
            Some(Ok(c)) if c.families.is_empty() => ListState::Empty,
            Some(Ok(_)) => ListState::Ready,
        })
    });

    // Every look at the rows: what stopped moving since the last one re-reads
    // the catalog at once, so its package turns "installed" as soon as its
    // own files are in. The catalog used to be re-read only once nothing
    // moved at all, and a package done before the others offered "Download"
    // again until the last of them landed.
    Effect::new(move |_| {
        let looked = catalog.with(|c| {
            downloads.with(|d| {
                let d = match d {
                    // A failed list read is no look: nothing stopped.
                    Some(Err(_)) => return None,
                    d => d.as_ref().and_then(|r| r.as_ref().ok()),
                };
                let c = c.as_ref().and_then(|r| r.as_ref().ok());
                Some(watching.with(|w| view_state::moving(c, d, w)))
            })
        });
        let Some(now) = looked else {
            return;
        };
        let fresh = landed.with_untracked(|l| {
            watching.with_untracked(|w| before.with_value(|b| view_state::landings(b, w, &now, l)))
        });
        before.set_value(now);
        if fresh.is_empty() {
            return;
        }
        let asked = reload.get_untracked() + 1;
        landed.update(|l| {
            l.extend(
                fresh
                    .into_iter()
                    .map(|(package, id)| Landed { package, id, asked }),
            )
        });
        reload.set(asked);
    });

    // A catalog read is in: the landed rows it was asked after are counted
    // in it, and their packages show what it says.
    Effect::new(move |_| {
        catalog.track();
        let answered = answered.get_value();
        let (waiting, watched) = landed
            .with_untracked(|l| watching.with_untracked(|w| view_state::settle(l, w, answered)));
        if waiting.len() != landed.with_untracked(Vec::len) {
            landed.set(waiting);
            watching.set(watched);
        }
    });

    // Poll only while something is actually moving — the old UI's rule, kept.
    // "Moving" is the last look's: a list read that failed keeps the poll
    // going rather than reading as everything done.
    let handle = set_interval_with_handle(
        move || {
            if open.get_untracked() && !before.with_value(Vec::is_empty) {
                dl_reload.update(|n| *n += 1);
            }
        },
        std::time::Duration::from_secs(2),
    );
    if let Ok(h) = handle {
        on_cleanup(move || h.clear());
    }

    let refresh = move |_| {
        if refreshing.get_untracked() {
            return;
        }
        refreshing.set(true);
        refresh_err.set(None);
        spawn_local(async move {
            let res = crate::api::post::<serde_json::Value, _>(
                "/api/op/audio_catalog",
                &json!({ "action": "refresh" }),
            )
            .await;
            refreshing.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("catalog refreshed")
                        .to_string());
                    reload.update(|n| *n += 1);
                }
                Err(e) => refresh_err.set(Some(e.to_string())),
            }
        });
    };

    let install = move |family: String, package: String| {
        spawn_local(async move {
            match crate::api::post::<AudioCatalogInstall, _>(
                "/api/op/audio_catalog",
                &json!({ "action": "download", "family": family, "package": package }),
            )
            .await
            {
                Ok(r) => {
                    let pkg = r.package.clone();
                    watching.update(|w| w.extend(r.downloads.iter().map(|d| (pkg.clone(), d.id))));
                    dl_reload.update(|n| *n += 1);
                    toasts.ok(r.message);
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    // Downloaded → the editor, prefilled from the package. Creating the row is
    // the user's click, exactly as it was on the old page.
    //
    // The family comes along, not just the package: its default voice becomes
    // the row's inline default preset, so a TTS row speaks one voice when a
    // request names none instead of sampling a new random speaker. The voices
    // the family ships are not copied into presets any more — lmgw knows them
    // by name from the spec and the package itself (`GET /v1/audio/voices`
    // lists them), and a preset per built-in voice only shadowed them.
    let create = move |f: AudioFamily, pkg: AudioPackage| {
        let mut m = super::models::blank_audio_model();
        m.model_id = pkg.suggested_model_id;
        m.family = f.family.clone();
        m.path = pkg.suggested_path;
        m.task = pkg.suggested_task;
        m.mode = pkg.suggested_mode;
        if !f.default_voice.is_empty() {
            m.default_voice_preset = Some(json!({ "voice_id": f.default_voice.clone() }));
        }
        open.set(false);
        editors.audio.set(Some(m));
    };

    view! {
        <div class="wiz cat">
            <div class="row" style="justify-content:space-between; align-items:baseline">
                <span class="dim">
                    {move || match catalog.get().and_then(|r| r.ok()) {
                        Some(c) if !c.fetched_at.is_empty() => {
                            format!(
                                "{} families · fetched {} {}",
                                c.families.len(),
                                c.fetched_at.get(0..10).unwrap_or_default(),
                                c.fetched_at.get(11..19).unwrap_or_default(),
                            )
                        }
                        _ => "audio.cpp model_specs — nothing fetched yet".to_string(),
                    }}
                </span>
                <button class="btn" disabled=move || refreshing.get() on:click=refresh>
                    {move || if refreshing.get() { "Refreshing…" } else { "Refresh catalog" }}
                </button>
            </div>

            {move || refresh_err.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
            {move || {
                let lines = warnings.get();
                let n = lines.len();
                let lines = lines.into_iter().map(|l| view! { <div>{l}</div> }).collect_view();
                // Offline, every repo fails alike: past a few, a count to
                // open rather than a wall pushing the list off the dialog.
                match n {
                    0 => ().into_any(),
                    1..=3 => view! { <div class="wiz-err">{lines}</div> }.into_any(),
                    _ => {
                        view! {
                            <details class="wiz-err">
                                <summary>{format!("{n} warnings from the last refresh")}</summary>
                                {lines}
                            </details>
                        }
                            .into_any()
                    }
                }
            }}

            {move || match list_state.get() {
                ListState::Loading => {
                    view! { <div class="dim" style="margin-top:12px">"Loading…"</div> }.into_any()
                }
                ListState::Failed(e) => {
                    view! { <div class="wiz-err">"Failed to load the catalog: " {e}</div> }
                        .into_any()
                }
                ListState::Empty => {
                    view! {
                        <div class="empty">
                            "No catalog cached yet — refresh to fetch audio.cpp's model specs."
                        </div>
                    }
                        .into_any()
                }
                // Keyed by family id: a re-read updates the rows in place,
                // so each `<details>` keeps its own family.
                ListState::Ready => {
                    view! {
                        <div class="cat-list" node_ref=list_ref>
                            <For each=move || order.get() key=|id| id.clone() let:id>
                                <FamilyBlock
                                    id=id
                                    catalog=catalog
                                    order=order
                                    open_fams=open_fams
                                    downloads=downloads
                                    watching=watching
                                    landed=landed
                                    install=install
                                    create=create
                                />
                            </For>
                        </div>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

#[component]
fn FamilyBlock(
    id: String,
    catalog: CatalogResource,
    order: RwSignal<Vec<String>>,
    open_fams: RwSignal<HashMap<String, bool>>,
    downloads: LocalResource<crate::api::Result<DownloadsView>>,
    watching: RwSignal<Vec<(String, i64)>>,
    landed: RwSignal<Vec<Landed>>,
    install: impl Fn(String, String) + Copy + Send + Sync + 'static,
    create: impl Fn(AudioFamily, AudioPackage) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    // This family as the catalog has it now. A re-read that left it as it was
    // (most of them) changes nothing on screen.
    let fam = {
        let id = id.clone();
        Memo::new(move |_| {
            catalog.with(|c| {
                c.as_ref()
                    .and_then(|r| r.as_ref().ok())
                    .and_then(|c| c.families.iter().find(|f| f.family == id).cloned())
            })
        })
    };
    let default = catalog.with_untracked(|c| {
        let families = c
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .map(|c| c.families.as_slice())
            .unwrap_or_default();
        order.with_untracked(|o| view_state::default_open(families, o, &id))
    });
    let hue = hue_for(&id);
    let shown_open = {
        let id = id.clone();
        move || open_fams.with(|m| m.get(&id).copied().unwrap_or(default))
    };
    // Only the user's own toggles are written: setting `open` from the map
    // fires `toggle` too, and that one agrees with what is shown already.
    let on_toggle = {
        let shown_open = shown_open.clone();
        move |ev: leptos::ev::Event| {
            let now = ev
                .current_target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .is_some_and(|el| el.has_attribute("open"));
            if shown_open() != now {
                open_fams.update(|m| {
                    m.insert(id.clone(), now);
                });
            }
        }
    };
    // Its own Memo, so a download landing (which changes the family) does
    // not rebuild the options block and close it.
    let options = Memo::new(move |_| now(fam, |f| f.options.clone()));
    let package_ids = move || {
        now(fam, |f| {
            f.packages.iter().map(|p| p.id.clone()).collect::<Vec<_>>()
        })
    };
    view! {
        <details class="cat-fam" prop:open=shown_open on:toggle=on_toggle>
            <summary>
                <span class="model-chip" style=format!("--hue:{hue}")>
                    <i></i>
                    {move || now(fam, |f| f.display_name.clone())}
                </span>
                {move || {
                    let c = now(fam, |f| f.category.clone());
                    (!c.is_empty()).then(|| view! { <span class="type-badge">{c}</span> })
                }}
                // Upstream's own word for how finished this family is —
                // worth knowing before a multi-GiB download.
                {move || {
                    let status = now(fam, |f| f.status.clone());
                    (!status.is_empty() && status != "supported")
                        .then(|| view! { <span class="chip warn">{status}</span> })
                }}
                <span class="dim mono-sm">{move || now(fam, |f| f.tasks.join(", "))}</span>
                <span class="spacer" style="flex:1"></span>
                {move || {
                    now(fam, |f| f.served)
                        .then(|| view! { <span class="chip ok"><span class="dot"></span>"serving"</span> })
                }}
                {move || {
                    now(fam, |f| f.any_installed && !f.served)
                        .then(|| view! { <span class="chip ok"><span class="dot"></span>"installed"</span> })
                }}
                // A row of this family that loads none of its packages: the
                // sentence says which row and why.
                {move || {
                    let note = now(fam, |f| f.serving_note.clone());
                    (!note.is_empty())
                        .then(|| {
                            view! {
                                <span class="chip warn" title=note>
                                    <span class="dot"></span>"row matches no package"
                                </span>
                            }
                        })
                }}
            </summary>
            <div class="cat-body">
                {move || {
                    let d = now(fam, |f| f.description.clone());
                    (!d.is_empty()).then(|| view! { <p class="dim">{d}</p> })
                }}
                {move || {
                    let s = now(fam, |f| f.summary.clone());
                    (!s.is_empty()).then(|| view! { <p class="dim">{s}</p> })
                }}
                {move || {
                    let tags = now(fam, |f| f.tags.join(" · "));
                    (!tags.is_empty()).then(|| view! { <div class="dim mini-note">{tags}</div> })
                }}
                {move || {
                    let languages = now(fam, |f| f.languages.join(", "));
                    (!languages.is_empty())
                        .then(|| view! { <div class="dim mini-note">"Languages: " {languages}</div> })
                }}
                {move || {
                    let (voices, default_voice) = now(fam, |f| {
                        (f.builtin_voices.join(", "), f.default_voice.clone())
                    });
                    (!voices.is_empty())
                        .then(|| {
                            let note = match default_voice.is_empty() {
                                true => format!("Built-in voices: {voices}"),
                                false => format!(
                                    "Built-in voices: {voices} (default: {default_voice})",
                                ),
                            };
                            view! { <div class="dim mini-note">{note}</div> }
                        })
                }}
                {move || {
                    let docs = now(fam, |f| f.docs.clone());
                    (!docs.is_empty())
                        .then(|| {
                            view! {
                                <div class="dim mini-note">
                                    "Docs: "
                                    {docs
                                        .into_iter()
                                        .map(|d| {
                                            let href = format!(
                                                "https://github.com/0xShug0/audio.cpp/blob/main/{d}",
                                            );
                                            view! {
                                                <a href=href target="_blank" rel="noreferrer">
                                                    {d}
                                                </a>
                                                " "
                                            }
                                        })
                                        .collect_view()}
                                </div>
                            }
                        })
                }}
                {move || view! { <FamilyOptions options=options.get()/> }}
                <For each=package_ids key=|p| p.clone() let:pid>
                    <PackageRow
                        family=fam
                        id=pid
                        downloads=downloads
                        watching=watching
                        landed=landed
                        install=install
                        create=create
                    />
                </For>
            </div>
        </details>
    }
}

/// The options a family declares, as audio.cpp's own spec states them. Three
/// groups because where a value belongs decides where it is written: `load`
/// and `session` are the model row's two JSON boxes, `request` is what a call
/// may carry — which is exactly the question the editor and the lab leave an
/// operator guessing at otherwise.
#[component]
pub(super) fn FamilyOptions(options: lmgw_api_types::AudioFamilyOptions) -> impl IntoView {
    let groups = [
        ("Load options", options.load),
        ("Session options", options.session),
        ("Request options", options.request),
    ];
    let groups: Vec<_> = groups
        .into_iter()
        .filter(|(_, list)| !list.is_empty())
        .collect();
    (!groups.is_empty()).then(|| {
        view! {
            <details class="cat-opts">
                <summary class="dim mini-note">"Options this family accepts"</summary>
                {groups
                    .into_iter()
                    .map(|(label, list)| {
                        view! {
                            <div class="dim mini-note" style="margin-top:6px">{label}</div>
                            // Every option, in the list's own scroller (the
                            // catalog, the editor): a box of its own inside
                            // that was a second scrollbar in the first.
                            <div>
                                {list
                                    .into_iter()
                                    .map(|o| view! { <OptionLine o=o/> })
                                    .collect_view()}
                            </div>
                        }
                    })
                    .collect_view()}
            </details>
        }
    })
}

/// One declared option: `name (type) = default` plus whatever bounds the spec
/// states, then its own sentence.
#[component]
fn OptionLine(o: lmgw_api_types::AudioFamilyOption) -> impl IntoView {
    let mut head = o.name.clone();
    if !o.kind.is_empty() {
        head.push_str(&format!(" ({})", o.kind));
    }
    if let Some(d) = &o.default {
        let d = match d {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        head.push_str(&format!(" = {d}"));
    }
    let mut bounds: Vec<String> = Vec::new();
    if o.required {
        bounds.push("required".into());
    }
    if !o.values.is_empty() {
        bounds.push(o.values.join(" | "));
    }
    match (o.min, o.max) {
        (Some(a), Some(b)) => bounds.push(format!("{a}–{b}")),
        (Some(a), None) => bounds.push(format!("≥ {a}")),
        (None, Some(b)) => bounds.push(format!("≤ {b}")),
        (None, None) => {}
    }
    let bounds = bounds.join(" · ");
    view! {
        <div class="opt-line">
            <div>
                <span class="mono-sm">{head}</span>
                {(!bounds.is_empty())
                    .then(|| view! { <span class="dim mono-sm">" · " {bounds}</span> })}
            </div>
            {(!o.description.is_empty())
                .then(|| view! { <div class="dim mini-note">{o.description.clone()}</div> })}
        </div>
    }
}

/// The meta line under a package: what it is, its files and size, where they
/// come from, and what stands in the way of installing it.
fn package_meta(p: &AudioPackage) -> String {
    let spec = match p.precision.is_empty() {
        true => p.format.clone(),
        false => format!("{} · {}", p.format, p.precision),
    };
    let mut meta = vec![
        spec,
        format!(
            "{} file{}",
            p.file_count,
            if p.file_count == 1 { "" } else { "s" }
        ),
    ];
    if !p.size.is_empty() {
        meta.push(p.size.clone());
    }
    if !p.repo.is_empty() {
        meta.push(p.repo.clone());
    }
    if p.gated {
        meta.push("gated — needs an HF token".to_string());
    }
    if !p.description.is_empty() {
        meta.push(p.description.clone());
    }
    if !p.unavailable_reason.is_empty() {
        meta.push(p.unavailable_reason.clone());
    }
    // Where it is not already the title of "not published yet": a repo that
    // could not be listed, a file nobody looked for yet, or a downloaded
    // package the hub has since dropped files of.
    if !p.availability_note.is_empty() && !view_state::download_blocked(p) {
        meta.push(p.availability_note.clone());
    }
    meta.join(" · ")
}

#[component]
fn PackageRow(
    family: Memo<Option<AudioFamily>>,
    id: String,
    downloads: LocalResource<crate::api::Result<DownloadsView>>,
    watching: RwSignal<Vec<(String, i64)>>,
    landed: RwSignal<Vec<Landed>>,
    install: impl Fn(String, String) + Copy + Send + Sync + 'static,
    create: impl Fn(AudioFamily, AudioPackage) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    // The package as its family has it now: a re-read that flips it to
    // downloaded updates this row where it is.
    let pkg = Memo::new(move |_| {
        family.with(|f| {
            f.as_ref()
                .and_then(|f| f.packages.iter().find(|p| p.id == id).cloned())
        })
    });
    let installed = move || now(pkg, |p| p.installed);
    let partial = move || now(pkg, |p| p.partial);
    // Downloaded once and short of files now — the spec grew (audio-class
    // gap 4): the download fetches only what is missing.
    let incomplete = move || now(pkg, |p| p.incomplete);
    let served = move || now(pkg, |p| p.served);
    let missing = move || now(pkg, |p| p.missing_files.join(", "));

    let live = move || {
        let dv = downloads.get().and_then(|r| r.ok());
        let watched = watching.get();
        let landed = landed.get();
        pkg.with(|p| {
            p.as_ref()
                .map(|p| live_of(p, dv.as_ref(), &watched, &landed))
                .unwrap_or(Live::Idle)
        })
    };

    view! {
        <div class="cat-pkg">
            <div class="cat-pkg-main">
                <span class="mono-sm">{move || now(pkg, |p| p.display_name.clone())}</span>
                {move || {
                    now(pkg, |p| p.recommended)
                        .then(|| view! { <span class="type-badge">"recommended"</span> })
                }}
                // The spec's pin, and whether downloads follow it (the
                // audio.catalog_revision setting).
                {move || {
                    let (pin, followed) = now(pkg, |p| (p.pinned_commit.clone(), p.pin_followed));
                    (!pin.is_empty())
                        .then(|| {
                            let short = pin.get(..7).unwrap_or(&pin).to_string();
                            let (label, title) = match followed {
                                true => (
                                    format!("pinned to {short} by the spec"),
                                    format!(
                                        "Downloads take commit {pin}, the one the audio.cpp spec \
                                         pins — not the latest upload to the repo",
                                    ),
                                ),
                                false => (
                                    format!("spec pins {short} · taking latest"),
                                    format!(
                                        "The audio.cpp spec pins commit {pin}; downloads take the \
                                         latest (main) because Settings → Runtimes → Audio → \
                                         Catalog downloads is 'latest'",
                                    ),
                                ),
                            };
                            view! { <span class="type-badge mono-sm" title=title>{label}</span> }
                        })
                }}
                // A revision the spec names that is not a commit (a tag, a
                // short hash): lmgw pins only to a full commit, so it takes
                // main — said here rather than left to the DTO.
                {move || {
                    let (pin, rev) = now(pkg, |p| (p.pinned_commit.clone(), p.revision.clone()));
                    let rev = rev.trim().to_string();
                    (pin.is_empty() && !rev.is_empty() && rev != "main")
                        .then(|| {
                            let title = format!(
                                "The audio.cpp spec names revision '{rev}' for this package. \
                                 lmgw follows a pin only when it is a full commit hash, so \
                                 downloads take the latest (main)",
                            );
                            view! {
                                <span class="type-badge mono-sm" title=title>
                                    {format!("spec names {rev} · lmgw takes main")}
                                </span>
                            }
                        })
                }}
                <span class="spacer" style="flex:1"></span>
                {move || match live() {
                    Live::Running(_) => {
                        view! { <span class="chip live"><span class="dot"></span>"downloading"</span> }
                            .into_any()
                    }
                    Live::Failed => {
                        view! { <span class="chip err"><span class="dot"></span>"failed"</span> }
                            .into_any()
                    }
                    Live::Finishing => {
                        view! { <span class="chip live"><span class="dot"></span>"finishing"</span> }
                            .into_any()
                    }
                    Live::Idle if served() && incomplete() => {
                        view! {
                            <span class="chip warn" title=format!("missing: {}", missing())>
                                <span class="dot"></span>"serving · incomplete"
                            </span>
                        }
                            .into_any()
                    }
                    Live::Idle if served() => {
                        let by = now(pkg, |p| p.served_by.join(", "));
                        let from = now(pkg, |p| p.downloaded_from.clone());
                        view! {
                            <span class="chip ok" title=format!("served by {by}\n{from}")>
                                <span class="dot"></span>"serving"
                            </span>
                        }
                            .into_any()
                    }
                    Live::Idle if installed() => {
                        let from = now(pkg, |p| p.downloaded_from.clone());
                        view! {
                            <span class="chip ok" title=from>
                                <span class="dot"></span>"downloaded"
                            </span>
                        }
                            .into_any()
                    }
                    Live::Idle if partial() => {
                        view! {
                            <span class="chip warn" title=format!("missing: {}", missing())>
                                <span class="dot"></span>"incomplete"
                            </span>
                        }
                            .into_any()
                    }
                    Live::Idle => ().into_any(),
                }}
                // A row points at this package's files but not at this
                // package: the sentence says which row and what settles it.
                {move || {
                    let why = now(pkg, |p| p.serving_unclear.clone());
                    (!why.is_empty())
                        .then(|| {
                            view! {
                                <span class="chip warn" title=why>
                                    <span class="dot"></span>"variant unclear"
                                </span>
                            }
                        })
                }}
                {move || {
                    let repo = now(pkg, |p| p.repo.clone());
                    if repo.is_empty() {
                        // Upstream's own words where it has them: several
                        // families are deliberately not redistributed, and
                        // the answer is a local conversion, not a wait.
                        let why = now(pkg, |p| p.unavailable_reason.clone());
                        let label = match why.is_empty() {
                            true => "no download source".to_string(),
                            false => "not distributed".to_string(),
                        };
                        return view! {
                            <span class="dim mono-sm" title=why>{label}</span>
                        }
                            .into_any();
                    }
                    // The spec is ahead of the published weights: the queue
                    // would refuse the download, so this says it where the
                    // button would be, with the files and the listing date.
                    let blocked = now(pkg, view_state::download_blocked);
                    if blocked && !matches!(live(), Live::Running(_) | Live::Finishing) {
                        let why = now(pkg, |p| p.availability_note.clone());
                        return view! {
                            <span class="dim mono-sm" title=why>"not published yet"</span>
                        }
                            .into_any();
                    }
                    match view_state::slot(live(), installed()) {
                        Slot::Progress(pct) => {
                            let label = match pct {
                                Some(p) => format!("{p} %"),
                                None => "queued…".to_string(),
                            };
                            view! { <span class="dim mono-sm">{label}</span> }.into_any()
                        }
                        Slot::Finishing => {
                            view! { <span class="dim mono-sm">"finishing…"</span> }.into_any()
                        }
                        Slot::Create => {
                            view! {
                                <button
                                    class="btn primary"
                                    title="Create an audio model from this package"
                                    on:click=move |_| {
                                        if let (Some(f), Some(p)) = (
                                            family.get_untracked(),
                                            pkg.get_untracked(),
                                        ) {
                                            create(f, p);
                                        }
                                    }
                                >
                                    "Create model"
                                </button>
                            }
                                .into_any()
                        }
                        Slot::Download => {
                            let (label, title) = match (incomplete(), partial()) {
                                (true, _) => (
                                    "Complete install",
                                    now(pkg, |p| {
                                        let along = match p.pin_followed {
                                            true => " — and any installed file from another \
                                                     commit than the pin whose bytes changed \
                                                     there, so the package is one commit \
                                                     again (an unchanged one is recorded at \
                                                     the pin)",
                                            false => "",
                                        };
                                        format!(
                                            "Download the {} file(s) this package lacks from \
                                             {repo}: {}{along}",
                                            p.missing_files.len(),
                                            p.missing_files.join(", ")
                                        )
                                    }),
                                ),
                                (_, true) => ("Resume download", format!("Download from {repo}")),
                                _ => ("Download", format!("Download from {repo}")),
                            };
                            view! {
                                <button
                                    class="btn"
                                    title=title
                                    on:click=move |_| install(
                                        now(family, |f| f.family.clone()),
                                        now(pkg, |p| p.id.clone()),
                                    )
                                >
                                    {label}
                                </button>
                            }
                                .into_any()
                        }
                    }
                }}
            </div>
            <div class="cat-pkg-meta dim mono-sm">{move || now(pkg, package_meta)}</div>
            {move || match live() {
                Live::Running(pct) => {
                    view! {
                        <div class="progress">
                            <i style=format!("width:{}%", pct.unwrap_or(2))></i>
                        </div>
                    }
                        .into_any()
                }
                _ => ().into_any(),
            }}
        </div>
    }
}
