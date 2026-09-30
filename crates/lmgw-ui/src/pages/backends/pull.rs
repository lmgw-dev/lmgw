//! **Pull update** (container-builds §8): a registry image in use whose tag
//! now serves something newer. The panel asks first — the download's size is
//! not known before it starts — then follows the `image_pull` job on the
//! jobs feed, and when it has ended reads how (`container_image_pull_status`)
//! and offers to recreate the containers still on the old image, one
//! per-model apply each, as after a build run (§6).

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::builds::{ContainerImagePullStatus, ImageUseKind};

use super::images::{registry_chip, short_digest};
use super::recreate::{use_targets, RecreateButton, UnmatchedNote};
use super::{sha7, use_bk, use_chips};
use crate::backends_api as api;
use crate::scope::Scope;
use crate::widgets::{Modal, ModalFooter};

/// What the panel is about.
#[derive(Clone, Debug, PartialEq)]
pub struct PullTarget {
    /// The fully qualified reference — `RegistryUpdate::reference`.
    pub reference: String,
    /// The pull's job once started; `None` while the panel still asks.
    pub job_id: Option<i64>,
}

#[component]
pub fn PullModal() -> impl IntoView {
    let bk = use_bk();
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = bk.pull.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && bk.pull.with_untracked(Option::is_some) {
            bk.pull.set(None);
        }
    });
    view! {
        <Modal open=open title="Pull update">
            {move || {
                bk.pull
                    .get()
                    .map(|t| {
                        view! { <PullBody reference=t.reference job=t.job_id open=open/> }
                    })
            }}
        </Modal>
    }
}

#[component]
fn PullBody(reference: String, job: Option<i64>, open: RwSignal<bool>) -> impl IntoView {
    let bk = use_bk();
    let toasts = bk.toasts;
    let scope = Scope::new();
    let job = RwSignal::new(job);
    let starting = RwSignal::new(false);
    let status = RwSignal::new(None::<Result<ContainerImagePullStatus, String>>);
    let ref_sv = StoredValue::new(reference.clone());

    // What the Images tab knows about this reference: the update check.
    let update = {
        let r = reference.clone();
        move || {
            bk.images.with(|i| {
                i.as_ref().and_then(|i| {
                    i.images
                        .iter()
                        .filter_map(|img| img.registry_update.as_ref())
                        .find(|u| u.reference == r)
                        .cloned()
                })
            })
        }
    };
    let live = {
        let r = reference.clone();
        Memo::new(move |_| {
            let j = job.get();
            bk.pulls.with(|m| {
                m.get(&r)
                    .filter(|l| j.is_none_or(|j| j == l.job_id))
                    .cloned()
            })
        })
    };

    let read_status = move || {
        let Some(id) = job.get_untracked() else {
            return;
        };
        scope.spawn(async move {
            let res = api::container_image_pull_status(id)
                .await
                .map_err(|e| e.to_string());
            status.set(Some(res));
        });
    };
    // Once now (a reopened panel), and again whenever the job leaves the
    // feed — how a finished job is learned of.
    read_status();
    Effect::new(move |prev: Option<bool>| {
        let now = live.with(Option::is_some);
        if prev == Some(true) && !now {
            read_status();
        }
        now
    });

    let start = move |_| {
        if starting.get_untracked() {
            return;
        }
        starting.set(true);
        let r = ref_sv.get_value();
        spawn_local(async move {
            let res = api::container_image_pull(r.clone()).await;
            starting.try_set(false);
            match res {
                Ok(s) => {
                    // Kept by the page, so the row can open the result
                    // again, even when this panel was closed meanwhile.
                    bk.pulled.try_update(|m| {
                        m.insert(r.clone(), s.job_id);
                    });
                    if scope.alive() {
                        job.set(Some(s.job_id));
                        read_status();
                    }
                }
                Err(e) => toasts.err(format!("pull {r}: {e}")),
            }
        });
    };
    let cancel = move |_| {
        if let Some(l) = live.get_untracked() {
            bk.cancel(l.job_id, ref_sv.get_value());
        }
    };

    let finished = move || {
        live.with(Option::is_none)
            && status.with(|s| {
                matches!(s, Some(Ok(st)) if matches!(st.status.as_str(), "done" | "failed" | "canceled"))
            })
    };
    let result = Memo::new(move |_| {
        status.with(|s| match s {
            Some(Ok(st)) => st.result.clone(),
            _ => None,
        })
    });
    let uses = Signal::derive(move || {
        result.with(|r| r.as_ref().map(|r| r.used_by.clone()).unwrap_or_default())
    });
    let targets = use_targets(uses);
    let updated = Signal::derive(move || result.with(|r| r.as_ref().is_some_and(|r| r.updated)));
    let reload = Callback::new(move |()| {
        bk.load_builds();
        bk.load_images();
    });

    view! {
        <p>
            <code class="mono-sm">{reference.clone()}</code>
        </p>
        // The update check's word, until the pull has ended: after it, the
        // result below says what the tag names now.
        {move || {
            (!finished())
                .then(&update)
                .flatten()
                .map(|u| {
                    let (c, l, t) = registry_chip(&u);
                    let remote = u.remote_digest.as_deref().map(short_digest);
                    view! {
                        <div class="bk-facts">
                            <span class=c title=t><span class="dot"></span>{l}</span>
                            {remote.map(|d| view! { <span class="dim">"registry " <code class="mono-sm">{d}</code></span> })}
                        </div>
                    }
                })
        }}
        // Asking.
        <Show when=move || job.with(Option::is_none)>
            <div class="notice warn">
                <b>"The download size is not known before the pull starts"</b>
                "A CUDA image is often several GB. It lands in podman's image store next to the one it replaces."
            </div>
            <p class="dim">
                "Nothing is restarted: running containers keep the old image until they are recreated, "
                "and class defaults and model overrides that name this tag use the new image from their next start. "
                "When the pull is done, this panel offers to recreate the running ones."
            </p>
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Not now"
                </button>
                <button class="btn primary" disabled=move || starting.get() on:click=start>
                    {move || if starting.get() { "Starting…" } else { "Pull update" }}
                </button>
            </ModalFooter>
        </Show>
        // Pulling. The footer stays put while the line under it moves.
        <Show when=move || live.with(Option::is_some)>
            <div class="bk-facts">
                <span class="chip live"><span class="dot"></span>"pulling"</span>
                {move || {
                    live.with(|l| l.as_ref().and_then(|l| l.detail.old_id.clone()))
                        .map(|o| view! { <span class="dim">"replacing " <code class="mono-sm">{sha7(&o)}</code></span> })
                }}
            </div>
            <pre class="preset bk-pull-line">
                {move || live.with(|l| l.as_ref().map(|l| l.detail.last_line.clone()).unwrap_or_default())}
            </pre>
            <ModalFooter>
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Close — it keeps going"
                </button>
                <button class="btn danger" on:click=cancel>
                    "Cancel pull"
                </button>
            </ModalFooter>
        </Show>
        // Waiting for the job to show up, or the first status read.
        <Show when=move || job.with(Option::is_some) && live.with(Option::is_none) && !finished()>
            {move || match status.get() {
                Some(Err(e)) => {
                    view! {
                        <div class="notice err row">
                            <span class="spacer">"How the pull went could not be read: " {e}</span>
                            <button class="btn ghost sm" on:click=move |_| read_status()>
                                "Retry"
                            </button>
                        </div>
                    }
                        .into_any()
                }
                _ => view! { <div class="dim">"Starting the pull…"</div> }.into_any(),
            }}
        </Show>
        // Ended.
        <Show when=finished>
            {move || {
                status
                    .get()
                    .and_then(Result::ok)
                    .map(|st| match st.status.as_str() {
                        "done" => {
                            let r = st.result.clone().unwrap_or_default();
                            let head = if r.updated {
                                format!(
                                    "Pulled: the tag now names {}{}.",
                                    sha7(&r.new_id),
                                    r.old_id
                                        .as_deref()
                                        .map(|o| format!(" (was {})", sha7(o)))
                                        .unwrap_or_default()
                                )
                            } else {
                                "Already the newest: the registry served the image that was here.".to_string()
                            };
                            let followers: Vec<_> = r
                                .used_by
                                .iter()
                                .filter(|u| {
                                    matches!(
                                        u.kind,
                                        ImageUseKind::ClassDefault | ImageUseKind::ModelOverride
                                    )
                                })
                                .cloned()
                                .collect();
                            let log = r.log.join("\n");
                            view! {
                                <div class="wiz-ok" style="margin-top:0">{head}</div>
                                {(!followers.is_empty())
                                    .then(|| {
                                        view! {
                                            <div class="bk-chips">
                                                <span class="dim">"use it from their next start"</span>
                                                {use_chips(&followers)}
                                            </div>
                                        }
                                    })}
                                <UnmatchedNote targets=targets/>
                                <details class="bk-pull-log">
                                    <summary class="dim">{format!("podman's output ({} lines)", r.log.len())}</summary>
                                    <pre class="preset">{log}</pre>
                                </details>
                            }
                                .into_any()
                        }
                        "canceled" => view! { <div class="dim">"The pull was canceled; nothing changed."</div> }.into_any(),
                        _ => {
                            view! {
                                <div class="wiz-err">
                                    "The pull failed: " {st.error.clone().unwrap_or_else(|| "no reason recorded".to_string())}
                                </div>
                            }
                                .into_any()
                        }
                    })
            }}
            <ModalFooter>
                <RecreateButton
                    targets=targets
                    offer=updated
                    still_on="the old image"
                    on_done=reload
                />
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Close"
                </button>
            </ModalFooter>
        </Show>
    }
}
