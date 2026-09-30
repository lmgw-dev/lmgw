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

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{AudioCatalog, AudioCatalogInstall, AudioFamily, AudioPackage, DownloadsView};
use serde_json::json;

use crate::fmt::hue_for;
use crate::widgets::use_toasts;

/// Live state of one package's files, joined from `/api/hf/downloads`.
#[derive(Clone, Copy, PartialEq)]
enum Live {
    /// Nothing of this package is moving.
    Idle,
    /// Queued or transferring; `Some(pct)` once a total is known.
    Running(Option<u64>),
    Failed,
}

/// `watching` is `(package id, download row id)` — a package queued a moment
/// ago has rows the catalog snapshot does not know about yet.
fn live_of(pkg: &AudioPackage, dv: Option<&DownloadsView>, watching: &[(String, i64)]) -> Live {
    let mine: Vec<i64> = watching
        .iter()
        .filter(|(p, _)| *p == pkg.id)
        .map(|(_, id)| *id)
        .collect();
    let Some(dv) = dv else {
        return match mine.is_empty() {
            true => Live::Idle,
            false => Live::Running(None),
        };
    };
    let rows: Vec<_> = dv
        .downloads
        .iter()
        .filter(|d| pkg.download_ids.contains(&d.id) || mine.contains(&d.id))
        .collect();
    if rows.is_empty() && !mine.is_empty() {
        return Live::Running(None);
    }
    if rows.iter().any(|d| d.status == "failed") {
        return Live::Failed;
    }
    let running: Vec<_> = rows
        .iter()
        .filter(|d| matches!(d.status.as_str(), "queued" | "downloading"))
        .collect();
    if running.is_empty() {
        return Live::Idle;
    }
    let pcts: Vec<u64> = running.iter().filter_map(|d| d.percent).collect();
    match pcts.len() == running.len() && !pcts.is_empty() {
        true => Live::Running(Some(pcts.iter().sum::<u64>() / pcts.len() as u64)),
        false => Live::Running(None),
    }
}

#[component]
pub fn AudioCatalogBrowser(open: RwSignal<bool>) -> impl IntoView {
    let toasts = use_toasts();
    let editors = expect_context::<super::models::Editors>();

    let reload = RwSignal::new(0u32);
    let catalog = LocalResource::new(move || {
        reload.get();
        crate::api::get::<AudioCatalog>("/api/audio/catalog")
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
    let was_running = RwSignal::new(false);

    // Reopening the browser re-reads both sides: a download may have finished
    // (or been started elsewhere) while it was closed.
    Effect::new(move |_| {
        if open.get() {
            reload.update(|n| *n += 1);
            dl_reload.update(|n| *n += 1);
        }
    });

    // Anything of this catalog still moving? Untracked: this drives the poll,
    // it must not subscribe it to its own writes.
    let running_now = move || {
        let watched: Vec<i64> = watching
            .get_untracked()
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        let rows = downloads
            .get_untracked()
            .and_then(|r| r.ok())
            .map(|v| v.downloads)
            .unwrap_or_default();
        let ids: Vec<i64> = catalog
            .get_untracked()
            .and_then(|r| r.ok())
            .map(|c| {
                c.families
                    .iter()
                    .flat_map(|f| f.packages.iter().flat_map(|p| p.download_ids.clone()))
                    .collect()
            })
            .unwrap_or_default();
        ids.iter().chain(watched.iter()).any(|id| {
            match rows.iter().find(|r| r.id == *id) {
                Some(r) => matches!(r.status.as_str(), "queued" | "downloading"),
                // Queued a moment ago and not in this (older) snapshot yet.
                None => watched.contains(id),
            }
        })
    };

    // Poll only while something is actually moving — the old UI's rule, kept.
    let handle = set_interval_with_handle(
        move || {
            if !open.get_untracked() {
                return;
            }
            if running_now() {
                was_running.set(true);
                dl_reload.update(|n| *n += 1);
            } else if was_running.get_untracked() {
                // Everything landed: drop the local watch list and re-read the
                // catalog so the packages flip to "installed".
                was_running.set(false);
                watching.set(Vec::new());
                dl_reload.update(|n| *n += 1);
                reload.update(|n| *n += 1);
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
                    was_running.set(true);
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
    // The family comes along, not just the package: a spec that names the
    // voices its family ships (`ui.builtin_voices`) can fill the preset map
    // too, and a TTS row without presets is the one that samples a new random
    // speaker on every request.
    let create = move |f: AudioFamily, pkg: AudioPackage| {
        let mut m = super::models::blank_audio_model();
        m.model_id = pkg.suggested_model_id;
        m.family = f.family.clone();
        m.path = pkg.suggested_path;
        m.task = pkg.suggested_task;
        m.mode = pkg.suggested_mode;
        for v in &f.builtin_voices {
            m.voice_presets
                .insert(v.clone(), json!({ "voice_id": v.clone() }));
        }
        if !f.default_voice.is_empty() && m.voice_presets.contains_key(&f.default_voice) {
            m.default_voice_preset = Some(json!(f.default_voice.clone()));
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

            {move || match catalog.get() {
                None => view! { <div class="dim" style="margin-top:12px">"Loading…"</div> }.into_any(),
                Some(Err(e)) => {
                    view! { <div class="wiz-err">"Failed to load the catalog: " {e.to_string()}</div> }
                        .into_any()
                }
                Some(Ok(c)) if c.families.is_empty() => {
                    view! {
                        <div class="empty">
                            "No catalog cached yet — refresh to fetch audio.cpp's model specs."
                        </div>
                    }
                        .into_any()
                }
                Some(Ok(c)) => {
                    // Families with something on disk open themselves (the old
                    // page's rule); with nothing installed anywhere the first
                    // one opens, so the browser never reads as a wall of
                    // closed rows.
                    let none_here = !c.families.iter().any(|f| f.any_installed || f.served);
                    view! {
                        <div class="cat-list">
                            {c
                                .families
                                .into_iter()
                                .enumerate()
                                .map(|(i, f)| {
                                    let open_it = f.any_installed || f.served
                                        || (none_here && i == 0);
                                    view! {
                                        <FamilyBlock
                                            f=f
                                            open_it=open_it
                                            downloads=downloads
                                            watching=watching
                                            install=install
                                            create=create
                                        />
                                    }
                                })
                                .collect_view()}
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
    f: AudioFamily,
    open_it: bool,
    downloads: LocalResource<crate::api::Result<DownloadsView>>,
    watching: RwSignal<Vec<(String, i64)>>,
    install: impl Fn(String, String) + Copy + Send + Sync + 'static,
    create: impl Fn(AudioFamily, AudioPackage) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    // The whole family travels to the package rows, because the editor
    // prefill reads more than the package: voices, and the family name.
    let fam_row = f.clone();
    let packages = f.packages.clone();
    let tasks = f.tasks.join(", ");
    let languages = f.languages.join(", ");
    let hue = hue_for(&f.family);
    let status = f.status.clone();
    let tags = f.tags.join(" · ");
    let summary = f.summary.clone();
    let docs = f.docs.clone();
    let voices = f.builtin_voices.join(", ");
    let default_voice = f.default_voice.clone();
    let options = f.options.clone();
    view! {
        <details class="cat-fam" open=open_it>
            <summary>
                <span class="model-chip" style=format!("--hue:{hue}")>
                    <i></i>
                    {f.display_name.clone()}
                </span>
                {(!f.category.is_empty()).then(|| view! { <span class="type-badge">{f.category.clone()}</span> })}
                // Upstream's own word for how finished this family is —
                // worth knowing before a multi-GiB download.
                {(!status.is_empty() && status != "supported")
                    .then(|| view! { <span class="chip warn">{status.clone()}</span> })}
                <span class="dim mono-sm">{tasks}</span>
                <span class="spacer" style="flex:1"></span>
                {f.served
                    .then(|| view! { <span class="chip ok"><span class="dot"></span>"serving"</span> })}
                {(f.any_installed && !f.served)
                    .then(|| view! { <span class="chip ok"><span class="dot"></span>"installed"</span> })}
            </summary>
            <div class="cat-body">
                {(!f.description.is_empty()).then(|| view! { <p class="dim">{f.description.clone()}</p> })}
                {(!summary.is_empty()).then(|| view! { <p class="dim">{summary.clone()}</p> })}
                {(!tags.is_empty())
                    .then(|| view! { <div class="dim mini-note">{tags.clone()}</div> })}
                {(!languages.is_empty())
                    .then(|| view! { <div class="dim mini-note">"Languages: " {languages}</div> })}
                {(!voices.is_empty())
                    .then(|| {
                        let note = match default_voice.is_empty() {
                            true => format!("Built-in voices: {voices}"),
                            false => format!(
                                "Built-in voices: {voices} (default: {default_voice})",
                            ),
                        };
                        view! { <div class="dim mini-note">{note}</div> }
                    })}
                {(!docs.is_empty())
                    .then(|| {
                        view! {
                            <div class="dim mini-note">
                                "Docs: "
                                {docs
                                    .iter()
                                    .map(|d| {
                                        let href = format!(
                                            "https://github.com/0xShug0/audio.cpp/blob/main/{d}",
                                        );
                                        view! {
                                            <a href=href target="_blank" rel="noreferrer">
                                                {d.clone()}
                                            </a>
                                            " "
                                        }
                                    })
                                    .collect_view()}
                            </div>
                        }
                    })}
                <FamilyOptions options=options/>
                <For each=move || packages.clone() key=|p| p.id.clone() let:p>
                    <PackageRow
                        family=fam_row.clone()
                        p=p
                        downloads=downloads
                        watching=watching
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

#[component]
fn PackageRow(
    family: AudioFamily,
    p: AudioPackage,
    downloads: LocalResource<crate::api::Result<DownloadsView>>,
    watching: RwSignal<Vec<(String, i64)>>,
    install: impl Fn(String, String) + Copy + Send + Sync + 'static,
    create: impl Fn(AudioFamily, AudioPackage) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let pkg = StoredValue::new(p.clone());
    let fam = StoredValue::new(family);
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
    let meta = meta.join(" · ");
    let installed = p.installed;
    let partial = p.partial;
    let served = p.served;
    let downloadable = !p.repo.is_empty();
    let repo = p.repo.clone();

    let live = move || {
        let dv = downloads.get().and_then(|r| r.ok());
        let watched = watching.get();
        pkg.with_value(|p| live_of(p, dv.as_ref(), &watched))
    };

    view! {
        <div class="cat-pkg">
            <div class="cat-pkg-main">
                <span class="mono-sm">{p.display_name.clone()}</span>
                {p.recommended.then(|| view! { <span class="type-badge">"recommended"</span> })}
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
                    Live::Idle if served => {
                        view! { <span class="chip ok"><span class="dot"></span>"serving"</span> }
                            .into_any()
                    }
                    Live::Idle if installed => {
                        view! { <span class="chip ok"><span class="dot"></span>"downloaded"</span> }
                            .into_any()
                    }
                    Live::Idle if partial => {
                        view! { <span class="chip warn"><span class="dot"></span>"incomplete"</span> }
                            .into_any()
                    }
                    Live::Idle => ().into_any(),
                }}
                {move || {
                    if !downloadable {
                        // Upstream's own words where it has them: several
                        // families are deliberately not redistributed, and
                        // the answer is a local conversion, not a wait.
                        let why = pkg.with_value(|p| p.unavailable_reason.clone());
                        let label = match why.is_empty() {
                            true => "no download source".to_string(),
                            false => "not distributed".to_string(),
                        };
                        return view! {
                            <span class="dim mono-sm" title=why>{label}</span>
                        }
                            .into_any();
                    }
                    match (live(), installed) {
                        (Live::Running(pct), _) => {
                            let label = match pct {
                                Some(p) => format!("{p} %"),
                                None => "queued…".to_string(),
                            };
                            view! { <span class="dim mono-sm">{label}</span> }.into_any()
                        }
                        (_, true) => {
                            view! {
                                <button
                                    class="btn primary"
                                    title="Create an audio model from this package"
                                    on:click=move |_| create(
                                        fam.get_value(),
                                        pkg.get_value(),
                                    )
                                >
                                    "Create model"
                                </button>
                            }
                                .into_any()
                        }
                        (_, false) => {
                            view! {
                                <button
                                    class="btn"
                                    title=format!("Download from {repo}")
                                    on:click=move |_| install(
                                        fam.with_value(|f| f.family.clone()),
                                        pkg.with_value(|p| p.id.clone()),
                                    )
                                >
                                    {if partial { "Resume download" } else { "Download" }}
                                </button>
                            }
                                .into_any()
                        }
                    }
                }}
            </div>
            <div class="cat-pkg-meta dim mono-sm">{meta}</div>
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
