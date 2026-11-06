//! The recipes list's Upscalers (chat-media-tools design §9): the ESRGAN
//! models lmgw ships an entry for, each queued into the image models dir's
//! `upscalers/` by `image_upscaler_add`, and the files already there. The
//! Chat's Upscale and the `image__upscale` tool use them.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{ImageUpscalerAddResult, ImageUpscalerEntry, ImageUpscalers};
use serde_json::json;

use crate::fmt::hue_for;
use crate::widgets::use_toasts;

/// The download rows the upscaler entries are waiting on.
pub(super) fn download_ids(u: &ImageUpscalers) -> Vec<i64> {
    u.shipped.iter().filter_map(|e| e.download_id).collect()
}

#[component]
pub(super) fn UpscalerSection(
    list: ImageUpscalers,
    models_dir_missing: bool,
    /// `(entry key, download row id)` queued from here, as the recipes'.
    watching: RwSignal<Vec<(String, i64)>>,
    was_running: RwSignal<bool>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let toasts = use_toasts();
    let add = move |key: String| {
        spawn_local(async move {
            match crate::api::post::<ImageUpscalerAddResult, _>(
                "/api/op/image_upscaler_add",
                &json!({ "key": key }),
            )
            .await
            {
                Ok(r) => {
                    if let Some(d) = &r.download {
                        let k = format!("upscaler:{}", r.key);
                        watching.update(|w| w.push((k, d.id)));
                        was_running.set(true);
                    }
                    reload.update(|n| *n += 1);
                    toasts.ok(r.message);
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    let dir = if list.dir.is_empty() {
        "upscalers/".to_string()
    } else {
        format!("{}/", list.dir.trim_end_matches('/'))
    };
    let on_disk = list.on_disk.clone();
    view! {
        <div class="upscaler-list" data-section="upscalers">
            <h3 class="cat-group-head">"Upscalers"</h3>
            <p class="dim mini-note">
                "An upscaler makes a picture larger (the Chat's Upscale, the image__upscale tool). "
                "It is one file in the image models dir's " <span class="mono-sm">{dir}</span>
                ", which every image model reads, a running one too: the image server looks in "
                "it again whenever it is asked to upscale, so the next Upscale finds a new file."
            </p>
            {list
                .shipped
                .into_iter()
                .map(|e| view! { <UpscalerCard e=e blocked=models_dir_missing add=add/> })
                .collect_view()}
            {(!on_disk.is_empty())
                .then(|| {
                    view! {
                        <div class="cat-pkg upscaler-disk">
                            <div class="dim mini-note">"On disk, by the name the tools take:"</div>
                            {on_disk
                                .iter()
                                .map(|f| {
                                    view! {
                                        <div class="cat-pkg-main">
                                            <span class="mono-sm">{f.name.clone()}</span>
                                            <span class="spacer" style="flex:1"></span>
                                            <span class="dim mono-sm">
                                                {crate::fmt::fmt_bytes(f.size_bytes)}
                                            </span>
                                        </div>
                                    }
                                })
                                .collect_view()}
                        </div>
                    }
                })}
        </div>
    }
}

#[component]
fn UpscalerCard(
    e: ImageUpscalerEntry,
    blocked: bool,
    add: impl Fn(String) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let hue = hue_for(&e.key);
    let key = e.key.clone();
    let status = if e.present {
        view! { <span class="chip ok"><span class="dot"></span>"on disk"</span> }.into_any()
    } else if e.downloading {
        view! { <span class="chip live"><span class="dot"></span>"downloading"</span> }.into_any()
    } else {
        view! { <span class="chip off"><span class="dot"></span>"missing"</span> }.into_any()
    };
    let (present, downloading) = (e.present, e.downloading);
    view! {
        <details class="cat-fam" open=!present data-upscaler=e.key.clone()>
            <summary>
                <span class="model-chip" style=format!("--hue:{hue}")>
                    <i></i>
                    {e.display_name.clone()}
                </span>
                <span class="dim mono-sm">{e.size.clone()}</span>
                <span class="type-badge">{format!("×{}", e.scale)}</span>
                <span class="spacer" style="flex:1"></span>
                {status}
            </summary>
            <div class="cat-body">
                <p class="dim">{e.description.clone()}</p>
                <div class="cat-pkg">
                    <div class="cat-pkg-main">
                        <span class="mono-sm">{e.file.clone()}</span>
                        {e.gated
                            .then(|| view! { <span class="chip warn"><span class="dot"></span>"gated"</span> })}
                        <span class="spacer" style="flex:1"></span>
                        <span class="type-badge licence-badge">{e.licence.clone()}</span>
                    </div>
                    <div class="cat-pkg-meta dim mono-sm">{e.repo.clone()}</div>
                    <div class="cat-pkg-meta dim">
                        "Lands in " <span class="mono-sm">{e.dest_path.clone()}</span>
                    </div>
                </div>
                <div class="row" style="justify-content:flex-end; margin-top:10px">
                    {(!present && !downloading)
                        .then(|| {
                            view! {
                                <button
                                    class="btn"
                                    disabled=blocked
                                    on:click=move |_| add(key.clone())
                                >
                                    "Add"
                                </button>
                            }
                        })}
                </div>
            </div>
        </details>
    }
}
