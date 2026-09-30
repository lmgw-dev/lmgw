//! The Images tab: every local image of the three engines — built here,
//! built by hand, or pulled — with where it came from and who uses it.
//! podman is the source of truth (§3 "Image"); lmgw keeps no image table.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::builds::{
    ContainerImage, ContainerImageTagArgs, DiskInfo, Engine, ImageUse, ImageUseKind, RegistryUpdate,
};

use super::builds::delete_image;
use super::{
    ago, engine_label, extra_short, is_moving_tag, local_ts, sha7, status_chip, use_bk, use_chips,
    Bk, PullTarget,
};
use crate::backends_api as api;
use crate::fmt::human_bytes;
use crate::scope::Scope;
use crate::widgets::{ConfirmButton, MenuItem, Modal, ModalFooter, RowMenu};

const COLS: u32 = 9;

/// The tag "Set as default" writes: a build's moving tag when the image
/// holds it (following the build is what a default wants), else its first
/// tag. `None` for an untagged image.
pub fn preferred_tag(img: &ContainerImage) -> Option<String> {
    if let (Some(p), Some(e)) = (&img.provenance, img.engine) {
        // Production's moving tag or a dev instance's.
        if let Some(m) = img.tags.iter().find(|t| is_moving_tag(t, e, &p.slug)) {
            return Some(m.clone());
        }
    }
    img.tags.first().cloned()
}

/// Where an unlabeled image came from: a registry pull (its first tag names
/// a registry host other than `localhost`), else built by hand.
pub fn external_kind(img: &ContainerImage) -> &'static str {
    let from_registry = img.tags.iter().any(|t| {
        let first = t.split('/').next().unwrap_or("");
        t.contains('/')
            && (first.contains('.') || first.contains(':'))
            && first != "localhost"
            && !first.starts_with("localhost:")
    });
    if from_registry {
        "registry"
    } else {
        "external"
    }
}

/// A registry update check as a status chip: `(class, label, tooltip)`.
pub fn registry_chip(u: &RegistryUpdate) -> (&'static str, &'static str, String) {
    let when = if u.checked_at.is_empty() {
        String::new()
    } else {
        format!(
            "\nchecked {} ({})",
            ago(&u.checked_at),
            local_ts(&u.checked_at)
        )
    };
    let digests = |u: &RegistryUpdate| {
        let remote = u
            .remote_digest
            .as_deref()
            .map(short_digest)
            .unwrap_or_else(|| "?".to_string());
        let local = if u.local_digests.is_empty() {
            "none recorded".to_string()
        } else {
            u.local_digests
                .iter()
                .map(|d| short_digest(d))
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!("\nregistry {remote} · here {local}")
    };
    if let Some(e) = &u.error {
        return (
            "chip off",
            "not checked",
            format!(
                "{}: the registry could not be compared — {e}{when}",
                u.reference
            ),
        );
    }
    if u.update_available {
        (
            "chip warn",
            "update",
            format!(
                "{} serves a newer image than this one{}{when}",
                u.reference,
                digests(u)
            ),
        )
    } else {
        (
            "chip ok",
            "newest",
            format!("{} serves this image{}{when}", u.reference, digests(u)),
        )
    }
}

/// `sha256:0123456789ab…` → `0123456789ab`.
pub fn short_digest(d: &str) -> String {
    d.strip_prefix("sha256:")
        .unwrap_or(d)
        .chars()
        .take(12)
        .collect()
}

/// `repo:tag` → (`repo:`, `tag`); a ref without a tag is all base.
pub fn split_ref(r: &str) -> (String, String) {
    match r.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') && !repo.is_empty() => {
            (format!("{repo}:"), tag.to_string())
        }
        _ => (String::new(), r.to_string()),
    }
}

#[component]
pub fn ImagesTab() -> impl IntoView {
    let bk = use_bk();
    // Keyed by image id (below), so a re-read list order only needs to be
    // stable enough to sort by, never an identity: lmgw's own first (by
    // build), then the rest by name.
    let image_ids = Memo::new(move |_| {
        let mut v = bk
            .images
            .with(|r| r.as_ref().map(|r| r.images.clone()).unwrap_or_default());
        v.sort_by(|a, b| {
            let key = |i: &ContainerImage| {
                (
                    i.provenance.is_none(),
                    i.engine,
                    i.tags.first().cloned().unwrap_or_default(),
                )
            };
            key(a).cmp(&key(b))
        });
        v.into_iter().map(|i| i.id).collect::<Vec<_>>()
    });
    let loaded = move || bk.images.with(Option::is_some);
    let empty = move || {
        bk.images
            .with(|r| r.as_ref().is_some_and(|r| r.images.is_empty()))
    };
    view! {
        {move || {
            bk.images_err
                .get()
                .map(|e| {
                    view! {
                        <div class="notice err row">
                            "Listing the images failed: "
                            {e}
                            <button class="btn ghost sm" on:click=move |_| bk.load_images()>
                                "Retry"
                            </button>
                        </div>
                    }
                })
        }}
        {move || {
            bk.images_notice
                .get()
                .map(|e| {
                    view! {
                        <div class="notice err row">
                            <span class="spacer">{e}</span>
                            <button class="btn ghost sm" on:click=move |_| bk.images_notice.set(None)>
                                "Dismiss"
                            </button>
                        </div>
                    }
                })
        }}
        <div class="fill-pane card pad0">
            <table class="data bk-images">
                <thead>
                    <tr>
                        <th>"Image"</th>
                        <th class="col-p3">"Engine"</th>
                        <th class="bk-early">"GPU backend"</th>
                        <th class="num-h">"Size"</th>
                        <th class="bk-early">"Created"</th>
                        <th class="col-p2">"Provenance"</th>
                        <th>"Used by"</th>
                        <th class="col-p3" title="The run that built it (lmgw's images), or the registry update check (pulled images in use)">
                            "Status"
                        </th>
                        <th></th>
                    </tr>
                </thead>
                <tbody>
                    <Show when=move || !loaded() && bk.images_err.with(Option::is_none)>
                        <tr>
                            <td colspan=COLS class="dim">"Loading…"</td>
                        </tr>
                    </Show>
                    <Show when=empty>
                        <tr>
                            <td colspan=COLS class="empty">
                                "No llama.cpp, audio.cpp or sd.cpp image on this machine yet — a build's first run makes one."
                            </td>
                        </tr>
                    </Show>
                    // Keyed by the image: a re-read updates the row in
                    // place, so an open menu, an armed confirm and a live
                    // pull survive it (the builds
                    // table does the same).
                    <For each=move || image_ids.get() key=|id| id.clone() let:id>
                        <ImageRow id=id bk=bk/>
                    </For>
                </tbody>
            </table>
        </div>
        {move || bk.images.with(|r| r.as_ref().map(|r| disk_line(&r.disk)))}
    }
}

/// `podman system df` and the build leftovers, for information only —
/// nothing system-wide is pruned from here (§11.3).
fn disk_line(d: &DiskInfo) -> impl IntoView {
    let orphans = d.buildah_orphans.clone();
    let n = orphans.len();
    let list = orphans.join("\n");
    view! {
        <div class="bk-foot">
            <span>"Images on disk " <b>{human_bytes(d.images_total)}</b></span>
            <span>{human_bytes(d.reclaimable)} " reclaimable"</span>
            {if n == 0 {
                view! { <span>"no build leftovers"</span> }.into_any()
            } else {
                view! {
                    <span class="status-warn" title=list>
                        {format!(
                            "{n} build leftover{} ({})",
                            if n == 1 { "" } else { "s" },
                            orphans.first().cloned().unwrap_or_default(),
                        )}
                        {(n > 1).then(|| format!(" +{}", n - 1))}
                    </span>
                }
                    .into_any()
            }}
            <span class="dim">"shown for information; nothing is pruned from here"</span>
        </div>
    }
}

/// The images table's row for image `id`. Mirrors `BuildRow`: kept
/// by id, not by its whole `Debug` text, and every cell reads this row's
/// image from the live list (a `Memo`), so the row element — and whatever it
/// holds (an open menu, an armed delete confirm, a live pull) — survives a
/// re-read instead of remounting on every field change.
#[component]
fn ImageRow(id: String, bk: Bk) -> impl IntoView {
    let row_id = id.clone();
    let img = Memo::new(move |prev: Option<&ContainerImage>| {
        bk.images
            .with(|r| {
                r.as_ref()
                    .and_then(|r| r.images.iter().find(|i| i.id == row_id).cloned())
            })
            .or_else(|| prev.cloned())
            .unwrap_or_default()
    });

    let head = Memo::new(move |_| {
        img.with(|i| match i.tags.split_first() {
            Some((first, rest)) => (first.clone(), rest.len()),
            None => (format!("<none> {}", sha7(&i.id)), 0),
        })
    });
    // The repository gives way first: the tag is what tells images apart.
    let pfx_base = Memo::new(move |_| split_ref(&head.get().0));
    let title = move || {
        img.with(|i| {
            let mut t = i.tags.join("\n");
            if !t.is_empty() {
                t.push('\n');
            }
            t.push_str(&format!("id {}", i.id));
            t
        })
    };
    let provenance = move || {
        img.with(|i| match &i.provenance {
            Some(p) => {
                let extras = p
                    .extras
                    .iter()
                    .map(|x| extra_short(&x.extra))
                    .collect::<Vec<_>>();
                let title = format!(
                    "{} @ {}\nbase {}{}",
                    p.repo,
                    p.git_ref,
                    p.base,
                    if extras.is_empty() {
                        String::new()
                    } else {
                        format!("\n+ {}", extras.join(", "))
                    }
                );
                let extras_line = if extras.is_empty() {
                    String::new()
                } else {
                    format!(" + {}", extras.join(" "))
                };
                // Another instance's image (a dev instance's, or production's
                // seen from a dev instance) is described by its labels alone:
                // its build is not one of ours, not a deleted one.
                let who = p
                    .build_name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| {
                        if p.other_instance {
                            format!("{} (another lmgw instance)", p.slug)
                        } else {
                            format!("{} (deleted build)", p.slug)
                        }
                    });
                let title = format!("build: {who}\n{title}");
                view! {
                    <span class="mono-sm" title=title>
                        {who} " " <span class="pfx">{sha7(&p.base)}</span> {extras_line}
                    </span>
                }
                .into_any()
            }
            None => {
                let kind = external_kind(i);
                let title = if kind == "registry" {
                    "pulled from a registry; lmgw did not build it"
                } else {
                    "no lmgw labels: built by hand (build.sh) or imported"
                };
                view! { <span class="dim" title=title>{kind}</span> }.into_any()
            }
        })
    };
    let used = Memo::new(move |_| img.with(|i| i.used_by.clone()));
    let use_list = move || {
        used.with(|u| {
            u.iter()
                .map(super::use_label)
                .collect::<Vec<_>>()
                .join(", ")
        })
    };
    let engine = Memo::new(move |_| img.with(|i| i.engine));
    let preferred = Memo::new(move |_| img.with(preferred_tag));
    let default_for = Memo::new(move |_| {
        used.with(|u| {
            u.iter()
                .filter(|u| u.kind == ImageUseKind::ClassDefault)
                .map(|u| u.class.clone())
                .collect::<Vec<_>>()
        })
    });
    let backend = Memo::new(move |_| img.with(|i| i.backend.clone()));
    let size = Memo::new(move |_| img.with(|i| i.size));
    let created = Memo::new(move |_| img.with(|i| i.created.clone()));
    let run_status = Memo::new(move |_| img.with(|i| i.run_status));
    let reference =
        Memo::new(move |_| img.with(|i| i.registry_update.as_ref().map(|u| u.reference.clone())));
    let can_pull = move || {
        img.with(|i| {
            i.registry_update
                .as_ref()
                .is_some_and(|u| u.update_available)
        })
    };
    let live_pull = Memo::new(move |_| {
        reference
            .get()
            .and_then(|r| bk.pulls.with(|m| m.get(&r).cloned()))
    });
    let pulled_job = move || {
        reference
            .get()
            .and_then(|r| bk.pulled.with(|m| m.get(&r).copied()))
    };
    let open_pull = move |job_id: Option<i64>| {
        if let Some(r) = reference.get_untracked() {
            bk.pull.set(Some(PullTarget {
                reference: r,
                job_id,
            }));
        }
    };
    let status_cell = move || {
        if let Some(l) = live_pull.get() {
            return view! {
                <span class="chip live" title=l.detail.last_line.clone()>
                    <span class="dot"></span>
                    "pulling"
                </span>
            }
            .into_any();
        }
        if let Some(s) = run_status.get() {
            return status_chip(s, None);
        }
        img.with(|i| match &i.registry_update {
            Some(u) => {
                let (c, l, t) = registry_chip(u);
                view! { <span class=c title=t><span class="dot"></span>{l}</span> }.into_any()
            }
            None => ().into_any(),
        })
    };

    let copy_id = id.clone();
    let navigate = StoredValue::new(leptos_router::hooks::use_navigate());
    let menu = Signal::derive(move || {
        let mut items = Vec::new();
        let default_for = default_for.get();
        let preferred = preferred.get();
        if let Some(e) = engine.get() {
            for class in e.classes() {
                let class: &'static str = class;
                let already = default_for.iter().any(|c| c == class);
                let tag = preferred.clone();
                let label = format!("Set as default for {class}");
                let mut item = MenuItem::new(label, move || {
                    if let Some(t) = tag.clone() {
                        set_default(bk, class, t);
                    }
                })
                .disabled(already || preferred.is_none());
                item = item.title(if already {
                    format!("already the {class} default")
                } else if let Some(t) = &preferred {
                    format!(
                        "Settings → Runtimes: {class} image = {t}. Running {class} models keep their image until they restart."
                    )
                } else {
                    "an untagged image cannot be named in a setting — tag it first".to_string()
                });
                items.push(item);
            }
        }
        // Benchmark design §8.2: the New benchmark modal with this image
        // picked — a llama-server image only, the one engine a run drives.
        if engine.get() == Some(Engine::Llama) {
            let tag = preferred.clone();
            items.push(
                MenuItem::new("Benchmark with this image…", move || {
                    if let Some(t) = tag.clone() {
                        let href = crate::pages::benchmarks::new_href(None, Some(&t));
                        navigate.with_value(|n| n(&href, Default::default()));
                    }
                })
                .disabled(preferred.is_none())
                .title(if preferred.is_some() {
                    "Run the benchmark suite on a local chat row with this image, to compare it with another build"
                } else {
                    "an untagged image cannot be named in a run — tag it first"
                }),
            );
        }
        items.push(
            MenuItem::new("Retag…", move || {
                img.with_untracked(|i| bk.retag.set(Some((i.id.clone(), i.tags.clone()))))
            })
            .title("Add or remove a tag"),
        );
        let copy_id = copy_id.clone();
        items.push(MenuItem::new("Copy ID", move || {
            let _ = window().navigator().clipboard().write_text(&copy_id);
        }));
        items
    });
    let del_id = id.clone();
    // Unused: the inline confirm. In use: a confirmation that says what
    // deleting it anyway stops, removes and leaves pointing nowhere.
    let delete_btn = move || {
        let id = del_id.clone();
        if used.with(Vec::is_empty) {
            view! {
                <ConfirmButton
                    label="Delete"
                    confirm="Delete image?"
                    title="Remove this image from podman's store"
                    on_confirm=Callback::new(move |()| delete_image(bk, id.clone()))
                />
            }
            .into_any()
        } else {
            let title = format!(
                "In use by {} — shows what deleting it would stop and remove, then asks",
                use_list()
            );
            view! {
                <button
                    class="btn ghost sm"
                    title=title
                    on:click=move |_| bk.image_delete.set(Some(id.clone()))
                >
                    "Delete…"
                </button>
            }
            .into_any()
        }
    };
    view! {
        <tr>
            <td class="clip mono-sm" title=title>
                <div class="id-cell">
                    <span class="pfx">{move || pfx_base.get().0}</span>
                    <span class="id-base">{move || pfx_base.get().1}</span>
                    {move || {
                        let more = head.get().1;
                        (more > 0).then(|| view! { <span class="count id-flag">{format!("+{more}")}</span> })
                    }}
                </div>
            </td>
            <td class="col-p3">
                {move || engine.get().map(engine_label).unwrap_or("—")}
            </td>
            <td class="bk-early mono-sm">{move || backend.get().unwrap_or_else(|| "—".to_string())}</td>
            <td class="num">{move || human_bytes(size.get())}</td>
            <td class="bk-early dim" title=move || local_ts(&created.get())>{move || ago(&created.get())}</td>
            <td class="col-p2 bk-cut-s">{provenance}</td>
            <td class="bk-cut">
                <span class="bk-chips" title=use_list>
                    {move || {
                        if used.with(Vec::is_empty) {
                            view! { <span class="dim">"unused"</span> }.into_any()
                        } else {
                            used.with(|u| use_chips(u)).into_any()
                        }
                    }}
                </span>
            </td>
            <td class="col-p3">{status_cell}</td>
            <td class="actions">
                <span class="row-acts">
                    {move || {
                        if let Some(l) = live_pull.get() {
                            let job = l.job_id;
                            Some(
                                view! {
                                    <button
                                        class="btn ghost sm"
                                        title="The pull's progress"
                                        on:click=move |_| open_pull(Some(job))
                                    >
                                        "Pulling…"
                                    </button>
                                }
                                    .into_any(),
                            )
                        } else if let Some(job) = pulled_job() {
                            Some(
                                view! {
                                    <button
                                        class="btn ghost sm"
                                        title="What the pull did, and the containers still on the old image"
                                        on:click=move |_| open_pull(Some(job))
                                    >
                                        "Pulled"
                                    </button>
                                }
                                    .into_any(),
                            )
                        } else if can_pull() {
                            Some(
                                view! {
                                    <button
                                        class="btn sm"
                                        title="Pull the registry's newer image for this tag — asks first"
                                        on:click=move |_| open_pull(None)
                                    >
                                        "Pull update"
                                    </button>
                                }
                                    .into_any(),
                            )
                        } else {
                            None
                        }
                    }}
                    {delete_btn}
                    <RowMenu items=menu/>
                </span>
            </td>
        </tr>
    }
}

fn set_default(bk: Bk, class: &'static str, tag: String) {
    spawn_local(async move {
        match api::set_class_default(class, &tag).await {
            Ok(v) => {
                let note = v
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(|m| format!(" — {m}"))
                    .unwrap_or_default();
                bk.toasts.ok(format!("{class} default is now {tag}{note}"));
                bk.load_builds();
                bk.load_images();
            }
            Err(e) => bk.toasts.err(format!("{class} default: {e}")),
        }
    });
}

/// Add or remove tags on one image (`container_image_tag`).
#[component]
pub fn RetagModal() -> impl IntoView {
    let bk = use_bk();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = bk.retag.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && bk.retag.with_untracked(Option::is_some) {
            bk.retag.set(None);
        }
    });
    view! {
        <Modal open=open title="Image tags">
            {move || bk.retag.get().map(|(id, tags)| view! { <RetagBody id=id tags=tags/> })}
        </Modal>
    }
}

#[component]
fn RetagBody(id: String, tags: Vec<String>) -> impl IntoView {
    let bk = use_bk();
    let tags = RwSignal::new(tags);
    let add = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let id_sv = StoredValue::new(id.clone());
    let send = move |args: ContainerImageTagArgs| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let res = api::container_image_tag(&args).await;
            busy.set(false);
            match res {
                Ok(r) => {
                    tags.set(r.tags);
                    add.set(String::new());
                    bk.toasts.ok("tags updated");
                    bk.load_builds();
                    bk.load_images();
                }
                Err(e) => bk.toasts.err(e.to_string()),
            }
        });
    };
    let do_add = move || {
        let t = add.get_untracked().trim().to_string();
        if t.is_empty() {
            return;
        }
        send(ContainerImageTagArgs {
            image: id_sv.get_value(),
            add: Some(t),
            remove: None,
        });
    };
    view! {
        <p class="dim">
            "Image " <code class="mono-sm">{sha7(&id)}</code>
            ". A tag is just a name for it: removing the last one leaves the image untagged, "
            "not deleted."
        </p>
        <div class="bk-taglist">
            <For each=move || tags.get() key=|t| t.clone() let:t>
                {
                    let tag = StoredValue::new(t.clone());
                    view! {
                        <div class="copy-line">
                            <code class="mono-sm spacer">{t.clone()}</code>
                            <ConfirmButton
                                label="Remove"
                                confirm="Remove tag?"
                                disabled=Signal::derive(move || busy.get())
                                on_confirm=Callback::new(move |()| {
                                    send(ContainerImageTagArgs {
                                        image: id_sv.get_value(),
                                        add: None,
                                        remove: Some(tag.get_value()),
                                    })
                                })
                            />
                        </div>
                    }
                }
            </For>
            <Show when=move || tags.with(Vec::is_empty)>
                <div class="dim">"untagged"</div>
            </Show>
        </div>
        <div class="field" style="margin-top:12px">
            <label>"Add a tag"</label>
            <div class="row" style="flex-wrap:nowrap">
                <input
                    class="input mono spacer"
                    placeholder=format!("{}:my-tag", Engine::Llama.image_repo())
                    prop:value=move || add.get()
                    on:input=move |ev| add.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            do_add();
                        }
                    }
                />
                <button
                    class="btn"
                    disabled=move || busy.get() || add.with(|a| a.trim().is_empty())
                    on:click=move |_| do_add()
                >
                    "Add"
                </button>
            </div>
        </div>
    }
}

/// Delete an image in use, or one picked from a run's menu: what uses it,
/// read fresh, and what deleting it anyway does about each — then the delete.
#[component]
pub fn ImageDeleteModal() -> impl IntoView {
    let bk = use_bk();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = bk.image_delete.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && bk.image_delete.with_untracked(Option::is_some) {
            bk.image_delete.set(None);
        }
    });
    view! {
        <Modal open=open title="Delete image">
            {move || bk.image_delete.get().map(|id| view! { <ImageDeleteBody id=id/> })}
        </Modal>
    }
}

fn is_container(u: &ImageUse) -> bool {
    matches!(
        u.kind,
        ImageUseKind::RunningContainer | ImageUseKind::StoppedContainer
    )
}

#[component]
fn ImageDeleteBody(id: String) -> impl IntoView {
    let bk = use_bk();
    // `None` while reading; `Ok(None)` when the image is gone already.
    let found = RwSignal::new(None::<Result<Option<ContainerImage>, String>>);
    let busy = RwSignal::new(false);
    let err = RwSignal::new(None::<String>);
    let id_sv = StoredValue::new(id.clone());
    let scope = Scope::new();
    let read = move || {
        found.set(None);
        scope.spawn(async move {
            let want = id_sv.get_value();
            let want = want.trim_start_matches("sha256:");
            let res = api::container_images(false)
                .await
                .map(|r| r.images.into_iter().find(|i| i.id == want))
                .map_err(|e| e.to_string());
            found.set(Some(res));
        });
    };
    read();
    let users = move || {
        found.with(|f| match f {
            Some(Ok(Some(i))) => i.used_by.clone(),
            _ => Vec::new(),
        })
    };
    let ready = move || found.with(|f| matches!(f, Some(Ok(Some(_)))));
    let delete = move |force: bool| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        err.set(None);
        spawn_local(async move {
            let id = id_sv.get_value();
            let res = api::container_image_delete(id.clone(), force).await;
            busy.set(false);
            match res {
                Ok(r) => {
                    bk.image_delete.set(None);
                    bk.images_notice.set(None);
                    bk.toasts.ok(if r.removed_containers.is_empty() {
                        format!("deleted {}", sha7(&id))
                    } else {
                        format!(
                            "deleted {} — stopped and removed {}",
                            sha7(&id),
                            r.removed_containers.join(", ")
                        )
                    });
                    if !r.still_named_by.is_empty() {
                        let names: Vec<String> =
                            r.still_named_by.iter().map(super::use_label).collect();
                        bk.toasts.warn(format!(
                            "{} still name{} the deleted image — point {} at another one before \
                             the next start",
                            names.join(", "),
                            if names.len() == 1 { "s" } else { "" },
                            if names.len() == 1 { "it" } else { "them" }
                        ));
                    }
                    bk.load_builds();
                    bk.load_images();
                }
                Err(e) => err.set(Some(e.to_string())),
            }
        });
    };
    let tags = move || {
        found.with(|f| match f {
            Some(Ok(Some(i))) if !i.tags.is_empty() => format!(" — {}", i.tags.join(", ")),
            _ => String::new(),
        })
    };
    view! {
        <p>
            "Image " <code class="mono-sm">{sha7(&id)}</code>
            <span class="mono-sm" style="overflow-wrap:anywhere">{tags}</span>
        </p>
        {move || match found.get() {
            None => view! { <p class="dim">"Checking what uses it…"</p> }.into_any(),
            Some(Err(e)) => {
                view! {
                    <div class="notice err row">
                        <span class="spacer">{format!("Could not tell what uses it: {e}")}</span>
                        <button class="btn ghost sm" on:click=move |_| read()>
                            "Retry"
                        </button>
                    </div>
                }
                    .into_any()
            }
            Some(Ok(None)) => {
                view! { <p class="dim">"It is no longer on this machine."</p> }.into_any()
            }
            Some(Ok(Some(img))) => delete_effects(&img.used_by).into_any(),
        }}
        {move || {
            err.get().map(|e| view! { <div class="notice err" style="margin-top:10px">{e}</div> })
        }}
        <ModalFooter>
            <button class="btn ghost" on:click=move |_| bk.image_delete.set(None)>
                "Keep it"
            </button>
            {move || {
                let force = !users().is_empty();
                view! {
                    <button
                        class="btn danger"
                        disabled=move || busy.get() || !ready()
                        on:click=move |_| delete(force)
                    >
                        {if force { "Delete anyway" } else { "Delete image" }}
                    </button>
                }
            }}
        </ModalFooter>
    }
}

/// What deleting the image does about each of its users: the containers are
/// stopped and removed first, the class defaults and model overrides keep
/// naming it — a missing image from then on.
fn delete_effects(users: &[ImageUse]) -> impl IntoView {
    if users.is_empty() {
        return view! { <p class="dim">"Nothing uses it."</p> }.into_any();
    }
    let (containers, config): (Vec<ImageUse>, Vec<ImageUse>) =
        users.iter().cloned().partition(is_container);
    let running = containers
        .iter()
        .any(|u| u.kind == ImageUseKind::RunningContainer);
    view! {
        <div class="notice warn">
            <b>"It is in use."</b>
            {(!containers.is_empty())
                .then(|| {
                    view! {
                        <span class="detail">
                            {if running {
                                "Stopped and removed first — a running model goes down now, even mid-request: "
                            } else {
                                "Removed first: "
                            }}
                            {use_chips(&containers)}
                        </span>
                    }
                })}
            {(!config.is_empty())
                .then(|| {
                    view! {
                        <span class="detail">
                            "Left naming it, so they point at a missing image until you pick another: "
                            {use_chips(&config)}
                        </span>
                    }
                })}
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::builds::ImageProvenance;

    fn img(tags: &[&str]) -> ContainerImage {
        ContainerImage {
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_default_follows_the_build_when_the_image_holds_its_moving_tag() {
        let mut i = img(&[
            "localhost/lmgw-llama-server:official-master-abc1234-def567",
            "localhost/lmgw-llama-server:official-master",
        ]);
        i.engine = Some(Engine::Llama);
        i.provenance = Some(ImageProvenance {
            slug: "official-master".into(),
            ..Default::default()
        });
        assert_eq!(
            preferred_tag(&i).as_deref(),
            Some("localhost/lmgw-llama-server:official-master")
        );
        // a dev instance's image holds the dev twin
        let mut dev = i.clone();
        dev.tags = vec![
            "localhost/lmgw-dev-llama-server:official-master-abc1234-def567".into(),
            "localhost/lmgw-dev-llama-server:official-master".into(),
        ];
        assert_eq!(
            preferred_tag(&dev).as_deref(),
            Some("localhost/lmgw-dev-llama-server:official-master")
        );
        // an older run: only its immutable tag
        i.tags.truncate(1);
        assert_eq!(
            preferred_tag(&i).as_deref(),
            Some("localhost/lmgw-llama-server:official-master-abc1234-def567")
        );
        assert_eq!(preferred_tag(&img(&[])), None);
    }

    #[test]
    fn an_image_ref_splits_into_its_repository_and_tag() {
        assert_eq!(
            split_ref("localhost/lmgw-llama-server:official-master"),
            (
                "localhost/lmgw-llama-server:".to_string(),
                "official-master".to_string()
            )
        );
        assert_eq!(
            split_ref("localhost:5000/x"),
            (String::new(), "localhost:5000/x".to_string())
        );
        assert_eq!(
            split_ref("<none> abc"),
            (String::new(), "<none> abc".to_string())
        );
    }

    #[test]
    fn a_registry_check_reads_as_update_newest_or_not_checked() {
        let mut u = RegistryUpdate {
            reference: "ghcr.io/o/audio.cpp:full-cuda12".into(),
            remote_digest: Some(format!("sha256:{}", "a".repeat(64))),
            local_digests: vec![format!("sha256:{}", "b".repeat(64))],
            update_available: true,
            ..Default::default()
        };
        let (c, l, t) = registry_chip(&u);
        assert_eq!((c, l), ("chip warn", "update"));
        assert!(
            t.contains(&format!(
                "registry {} · here {}",
                "a".repeat(12),
                "b".repeat(12)
            )),
            "{t}"
        );
        u.update_available = false;
        assert_eq!(registry_chip(&u).1, "newest");
        u.error = Some("the registry wants credentials".into());
        let (c, l, t) = registry_chip(&u);
        assert_eq!((c, l), ("chip off", "not checked"));
        assert!(t.contains("wants credentials"));
        assert_eq!(short_digest("sha256:0123456789abcdef"), "0123456789ab");
    }

    #[test]
    fn a_registry_pull_is_told_apart_from_a_hand_build() {
        assert_eq!(
            external_kind(&img(&["ghcr.io/x/audio.cpp:latest"])),
            "registry"
        );
        assert_eq!(external_kind(&img(&["docker.io/library/x:1"])), "registry");
        assert_eq!(
            external_kind(&img(&["localhost/llama-server-cuda:ik-latest"])),
            "external"
        );
        assert_eq!(external_kind(&img(&["localhost:5000/x:1"])), "external");
        assert_eq!(external_kind(&img(&[])), "external");
    }
}
