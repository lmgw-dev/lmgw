//! "Add from recipe" — the image class's twin of the audio catalog, with a
//! shipped source instead of a fetched one.
//!
//! A stable-diffusion.cpp pipeline spans repos (diffusion weights here, a VAE
//! there, a text encoder somewhere else), so there is nothing upstream to
//! browse: the list is compiled into the gateway (image-generation §7.2) and
//! joined server-side against what is on disk and what is downloading. Adding
//! one queues every missing component through the same download queue
//! everything else uses, and hands a **prefilled row** to the image editor —
//! the row is never created here, because `image_model_set` refuses files
//! that are not on disk yet, and that refusal is what keeps a broken row out
//! of the table.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{
    DownloadsView, ImageRecipe, ImageRecipeAddResult, ImageRecipeRow, ImageRecipes,
};
use serde_json::{json, Value};

use crate::fmt::hue_for;
use crate::widgets::{use_toasts, Select};

/// Live state of one recipe's components, joined from `/api/hf/downloads`.
#[derive(Clone, Copy, PartialEq)]
enum Live {
    /// Nothing of this recipe is moving.
    Idle,
    /// Queued or transferring; `Some(pct)` once every running row has a total.
    Running(Option<u64>),
    Failed,
}

/// Is *this* pipeline on its way?
///
/// `watching` is `(recipe key, download row id)` — a recipe added a moment ago
/// has rows the recipes snapshot does not know about yet.
///
/// The subtle half is that components are **shared on purpose**: six recipes
/// name the same FLUX autoencoder, two the same Qwen encoder. So "a component
/// of mine has a live download row" does not mean this recipe is being
/// fetched — adding Z-Image-Turbo used to light up every FLUX card with a
/// progress bar and swallow its Add button, because the one shared VAE was
/// moving. A card therefore claims progress only when **nothing of it is
/// still un-queued** (every component is on disk or in flight), or when this
/// browser queued the rows itself a moment ago. A neighbour's transfer is
/// still visible where it is true — on the shared component's own row, next
/// to the names it is shared with.
fn live_of(r: &ImageRecipe, dv: Option<&DownloadsView>, watching: &[(String, i64)]) -> Live {
    let watched: Vec<i64> = watching
        .iter()
        .filter(|(k, _)| *k == r.key)
        .map(|(_, id)| *id)
        .collect();
    let all_accounted_for = !r.components.iter().any(|c| !c.present && !c.downloading);
    if watched.is_empty() && !all_accounted_for {
        return Live::Idle;
    }
    let mut ids: Vec<i64> = r.components.iter().filter_map(|c| c.download_id).collect();
    ids.extend(watched);
    if ids.is_empty() {
        return Live::Idle;
    }
    let Some(dv) = dv else {
        return Live::Running(None);
    };
    let rows: Vec<_> = dv
        .downloads
        .iter()
        .filter(|d| ids.contains(&d.id))
        .collect();
    if rows.is_empty() {
        // Queued a moment ago and not in this (older) snapshot yet.
        return match r.components.iter().any(|c| c.downloading) {
            true => Live::Running(None),
            false => Live::Idle,
        };
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

/// The prefilled row the editor opens on. Everything else on the row is the
/// class default `blank_image_model` already states, so only the six fields
/// `image_recipe_add` actually decided are copied over.
fn seed_from(row: ImageRecipeRow) -> lmgw_api_types::ImageModel {
    let mut m = super::models::blank_image_model();
    m.model_id = row.model_id;
    m.files = row.files;
    m.args = row.args;
    m.modes = row.modes;
    m.edit = row.edit;
    m
}

#[component]
pub fn ImageRecipeBrowser(open: RwSignal<bool>) -> impl IntoView {
    let toasts = use_toasts();
    let editors = expect_context::<super::models::Editors>();

    let reload = RwSignal::new(0u32);
    // A read, but an ops verb, so a POST: `image_recipes` is shared with
    // `lmgw__image_recipes` and the whole `/api/op` plane is POST-only.
    let recipes = LocalResource::new(move || {
        reload.get();
        async move { crate::api::post::<ImageRecipes, _>("/api/op/image_recipes", &json!({})).await }
    });
    let dl_reload = RwSignal::new(0u32);
    let downloads = LocalResource::new(move || {
        dl_reload.get();
        crate::api::get::<DownloadsView>("/api/hf/downloads")
    });
    // `(recipe key, download row id)` queued from here, so a recipe turns
    // amber before the first progress poll has come back.
    let watching = RwSignal::new(Vec::<(String, i64)>::new());
    let was_running = RwSignal::new(false);
    // Per recipe: which diffusion file the alternatives picker chose. Empty
    // means the recipe's own default.
    let chosen = RwSignal::new(Vec::<(String, String)>::new());

    // Reopening the browser re-reads both sides: a download may have finished
    // (or been started elsewhere) while it was closed.
    Effect::new(move |_| {
        if open.get() {
            reload.update(|n| *n += 1);
            dl_reload.update(|n| *n += 1);
        }
    });

    // Anything of this list still moving? Untracked: this drives the poll, it
    // must not subscribe it to its own writes.
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
        let ids: Vec<i64> = recipes
            .get_untracked()
            .and_then(|r| r.ok())
            .map(|v| {
                v.recipes
                    .iter()
                    .flat_map(|r| r.components.iter().filter_map(|c| c.download_id))
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

    // Poll only while something is actually moving — the audio catalog's rule.
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
                // recipes so they flip to "installed".
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

    let add = move |key: String| {
        let file = chosen
            .get_untracked()
            .into_iter()
            .find(|(k, _)| *k == key)
            .map(|(_, f)| f)
            .filter(|f| !f.is_empty());
        spawn_local(async move {
            let mut body = json!({ "key": key });
            if let Some(f) = file {
                body["diffusion_file"] = Value::from(f);
            }
            match crate::api::post::<ImageRecipeAddResult, _>("/api/op/image_recipe_add", &body)
                .await
            {
                Ok(r) => {
                    let k = r.key.clone();
                    watching.update(|w| w.extend(r.downloads.iter().map(|d| (k.clone(), d.id))));
                    was_running.set(true);
                    dl_reload.update(|n| *n += 1);
                    reload.update(|n| *n += 1);
                    if r.files_queued == 0 {
                        // Nothing to wait for — go straight to the editor with
                        // the row it just handed back.
                        open.set(false);
                        editors.image.set(Some(seed_from(r.row)));
                    }
                    toasts.ok(r.message);
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    // Downloaded → the editor, prefilled. `image_recipe_add` is what renders
    // the row (it resolves the chosen quant into the `files` map), so this
    // calls it again rather than rebuilding the row in the browser: with every
    // component already on disk it queues nothing.
    let create = move |key: String| add(key);

    view! {
        <div class="wiz cat">
            {move || match recipes.get() {
                None => view! { <div class="dim">"Loading…"</div> }.into_any(),
                Some(Err(e)) => {
                    view! {
                        <div class="wiz-err">"Failed to load the recipes: " {e.to_string()}</div>
                    }
                        .into_any()
                }
                Some(Ok(v)) => {
                    let models_dir_missing = v.models_dir_missing;
                    let models_dir = v.models_dir.clone();
                    let hf_token_set = v.hf_token_set;
                    let next_step = v.next_step.clone();
                    let list = v.recipes;
                    // Nothing installed anywhere: open the first card, so the
                    // browser never reads as a wall of closed rows.
                    let none_here = !list.iter().any(|r| r.installed || r.partial || r.served);
                    view! {
                        {models_dir_missing
                            .then(|| {
                                view! {
                                    <div class="notice warn">
                                        <b>"No image models directory"</b>
                                        <span class="detail">
                                            "Nothing can be downloaded or found until the image "
                                            "class has a models directory."
                                        </span>
                                        <a class="btn" href=super::settings::href("image.models_dir")>
                                            "Settings → Runtimes → Image"
                                        </a>
                                    </div>
                                }
                            })}
                        {(!models_dir_missing)
                            .then(|| {
                                view! {
                                    <div class="dim mono-sm">{models_dir.clone()}</div>
                                }
                            })}
                        <div class="cat-list">
                            {list
                                .into_iter()
                                .enumerate()
                                .map(|(i, r)| {
                                    // `partial` is true as soon as *any*
                                    // component is on disk, and most of those
                                    // are shared — installing one recipe made
                                    // six cards call themselves partial and
                                    // spring open. What says "this pipeline
                                    // has been started" is its **loader**: the
                                    // DiT or checkpoint nothing else points at.
                                    let started = r
                                        .components
                                        .iter()
                                        .find(|c| {
                                            matches!(
                                                c.role.as_str(),
                                                "model" | "diffusion_model"
                                            )
                                        })
                                        .is_some_and(|c| c.present || c.downloading);
                                    let open_it = r.installed || r.served || started
                                        || (none_here && i == 0);
                                    view! {
                                        <RecipeCard
                                            r=r
                                            open_it=open_it
                                            started=started
                                            hf_token_set=hf_token_set
                                            next_step=next_step.clone()
                                            downloads=downloads
                                            watching=watching
                                            chosen=chosen
                                            add=add
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
fn RecipeCard(
    r: ImageRecipe,
    open_it: bool,
    /// The loader component is on disk or moving — this pipeline has actually
    /// been started, as opposed to merely sharing a file with one that was.
    started: bool,
    hf_token_set: bool,
    next_step: String,
    downloads: LocalResource<crate::api::Result<DownloadsView>>,
    watching: RwSignal<Vec<(String, i64)>>,
    chosen: RwSignal<Vec<(String, String)>>,
    add: impl Fn(String) + Copy + Send + Sync + 'static,
    create: impl Fn(String) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let recipe = StoredValue::new(r.clone());
    let key = r.key.clone();
    let hue = hue_for(&r.key);
    let components = r.components.clone();
    let installed = r.installed;
    let partial = r.partial;
    let served = r.served;
    // A gated component with no token is a refusal `image_recipe_add` makes
    // before writing anything, so say so before the click rather than after.
    let gated_blocked = !hf_token_set && r.components.iter().any(|c| c.gated && !c.present);

    let live = move || {
        let dv = downloads.get().and_then(|res| res.ok());
        let watched = watching.get();
        recipe.with_value(|r| live_of(r, dv.as_ref(), &watched))
    };

    view! {
        <details class="cat-fam" open=open_it>
            <summary>
                <span class="model-chip" style=format!("--hue:{hue}")>
                    <i></i>
                    {r.display_name.clone()}
                </span>
                <span class="dim mono-sm">{r.total_size.clone()}</span>
                <span class="spacer" style="flex:1"></span>
                {served
                    .then(|| view! { <span class="chip ok"><span class="dot"></span>"served"</span> })}
                {(installed && !served)
                    .then(|| view! { <span class="chip ok"><span class="dot"></span>"installed"</span> })}
                {(partial && started && !installed)
                    .then(|| view! { <span class="chip warn"><span class="dot"></span>"partial"</span> })}
            </summary>
            <div class="cat-body">
                <p class="dim">{r.description.clone()}</p>
                <div class="dim mini-note">{r.vram_note.clone()}</div>
                {gated_blocked
                    .then(|| {
                        view! {
                            <div class="notice warn">
                                <b>"Needs a Hugging Face token"</b>
                                <span class="detail">
                                    "A component of this pipeline is gated and the hub refuses "
                                    "it without a token that accepted the licence."
                                </span>
                                <span class="detail mono-sm">{next_step.clone()}</span>
                                <a class="btn" href=super::settings::href("hf_token")>
                                    "Settings → Tokens & updates"
                                </a>
                            </div>
                        }
                    })}
                <For
                    each=move || components.clone()
                    key=|c| (c.role.clone(), c.file.clone())
                    let:c
                >
                    <div class="cat-pkg">
                        <div class="cat-pkg-main">
                            <span class="type-badge">{c.role.clone()}</span>
                            <span class="mono-sm">{c.file.clone()}</span>
                            {c.gated
                                .then(|| view! { <span class="chip warn"><span class="dot"></span>"gated"</span> })}
                            <span class="spacer" style="flex:1"></span>
                            <span class="dim mono-sm">{c.size.clone()}</span>
                            {if c.present {
                                view! { <span class="chip ok"><span class="dot"></span>"on disk"</span> }
                                    .into_any()
                            } else if c.downloading {
                                view! { <span class="chip live"><span class="dot"></span>"downloading"</span> }
                                    .into_any()
                            } else if c.done {
                                view! { <span class="chip ok"><span class="dot"></span>"done"</span> }
                                    .into_any()
                            } else {
                                view! { <span class="chip off"><span class="dot"></span>"missing"</span> }
                                    .into_any()
                            }}
                        </div>
                        <div class="cat-pkg-meta dim mono-sm">{c.repo.clone()}</div>
                        {(!c.note.is_empty())
                            .then(|| view! { <div class="cat-pkg-meta dim">{c.note.clone()}</div> })}
                        {(!c.shared_with.is_empty())
                            .then(|| {
                                // Why a file can be on disk, or moving, under a
                                // recipe nobody clicked: it is one file, and
                                // these rows all point at it. Set apart from
                                // the component's own note, because this is
                                // the line that explains a surprise.
                                let names = c.shared_with.join(", ");
                                view! {
                                    <div class="cat-pkg-shared dim">
                                        "Shared with " {names}
                                        " — one file, fetched once. A transfer on this row "
                                        "may have been started from one of those."
                                    </div>
                                }
                            })}
                        {(!c.alternatives.is_empty()
                            && matches!(c.role.as_str(), "diffusion_model" | "model"))
                            .then(|| {
                                // Only for the component that loads the
                                // pipeline: `image_recipe_add` takes exactly
                                // one `diffusion_file` and resolves it against
                                // the primary role, so a picker on any other
                                // component would send a file the server
                                // correctly refuses.
                                let key = key.clone();
                                let current = c.file.clone();
                                let sel = RwSignal::new(String::new());
                                Effect::new(move |_| {
                                    let v = sel.get();
                                    let key = key.clone();
                                    chosen
                                        .update(|rows| {
                                            rows.retain(|(k, _)| *k != key);
                                            if !v.is_empty() {
                                                rows.push((key, v));
                                            }
                                        });
                                });
                                let alts = c.alternatives.clone();
                                let opts = Signal::derive(move || {
                                    std::iter::once((
                                        String::new(),
                                        format!("{current}  ·  recipe default"),
                                    ))
                                        .chain(
                                            alts
                                                .iter()
                                                .map(|a| {
                                                    (
                                                        a.file.clone(),
                                                        format!(
                                                            "{}  ·  {}{}",
                                                            a.label,
                                                            a.size,
                                                            if a.present { "  ·  on disk" } else { "" },
                                                        ),
                                                    )
                                                }),
                                        )
                                        .collect::<Vec<_>>()
                                });
                                view! {
                                    <div class="field" style="margin-top:6px">
                                        <label>"Quantization"</label>
                                        <Select value=sel options=opts/>
                                    </div>
                                }
                            })}
                    </div>
                </For>
                <div class="row" style="justify-content:flex-end; margin-top:10px">
                    {move || match live() {
                        Live::Running(pct) => {
                            let label = match pct {
                                Some(p) => format!("{p} %"),
                                None => "queued…".to_string(),
                            };
                            view! {
                                <div style="flex:1">
                                    <span class="dim mono-sm">{label}</span>
                                    <div class="progress">
                                        <i style=format!("width:{}%", pct.unwrap_or(2))></i>
                                    </div>
                                </div>
                            }
                                .into_any()
                        }
                        Live::Failed => {
                            view! {
                                <span class="dim mono-sm">
                                    "a component failed — see Downloads"
                                </span>
                            }
                                .into_any()
                        }
                        Live::Idle => ().into_any(),
                    }}
                    {move || {
                        let key = recipe.with_value(|r| r.key.clone());
                        if matches!(live(), Live::Running(_)) {
                            return ().into_any();
                        }
                        if installed {
                            return view! {
                                <button
                                    class="btn primary"
                                    title="Open the image editor prefilled with this pipeline"
                                    on:click=move |_| create(key.clone())
                                >
                                    "Create model"
                                </button>
                            }
                                .into_any();
                        }
                        view! {
                            <button
                                class="btn"
                                disabled=gated_blocked
                                on:click=move |_| add(key.clone())
                            >
                                {if partial { "Fetch what is missing" } else { "Add" }}
                            </button>
                        }
                            .into_any()
                    }}
                </div>
            </div>
        </details>
    }
}
