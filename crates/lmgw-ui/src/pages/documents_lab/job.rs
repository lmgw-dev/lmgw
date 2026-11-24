//! An async run's job, live (WP12): read from the public route
//! (`GET /v1/documents/jobs/{id}`, the session is the owner, who sees every
//! job) at a steady pace while it runs: its state, each file's state and
//! parts, and the tool pool's running and waiting steps the route reports.
//! Cancel is `DELETE` on the same route. The group's run lands once the job
//! has ended: the history follows the group until it is no longer in flight.

use leptos::prelude::*;
use lmgw_api_types::documents::jobs::ConvertJob;
use lmgw_api_types::documents_lab::JOBS_ENDPOINT;

use super::words;
use crate::fmt::fmt_bytes;
use crate::scope::Scope;
use crate::widgets::use_toasts;

/// How long the view waits between two reads of a running job: a pace, not
/// a bound; it reads until the job has ended, is gone, or the page is left.
const JOB_PACE_MS: i32 = 500;
/// How many reads in a row must answer "no such job" before the job counts
/// as gone (one answer can fall between a restart's two states).
const GONE_AFTER: u32 = 3;

/// Whether an error says the gateway has no such job.
fn is_gone(e: &crate::api::Error) -> bool {
    matches!(e, crate::api::Error::Api(a) if a.code == "job_not_found" || a.code == "not_found")
}

async fn pause(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(w) = web_sys::window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// The job's line: "running · 3 of 12 parts · 2 of 4 files".
pub(super) fn job_line(j: &ConvertJob) -> String {
    let done = j.documents.iter().filter(|f| f.state.has_result()).count();
    let mut s = format!(
        "{} · {} of {}",
        words::state(j.state.as_str()),
        done,
        words::count(j.files, "file", "files")
    );
    if j.parts > 0 {
        s.push_str(&format!(" · {} of {} parts", j.parts_done, j.parts));
    }
    s
}

/// The live job `id`.
#[component]
pub(super) fn JobView(id: String) -> impl IntoView {
    let scope = Scope::new();
    let toasts = use_toasts();
    let job = RwSignal::new(None::<ConvertJob>);
    let err = RwSignal::new(None::<String>);
    let gone = RwSignal::new(false);
    let href = format!("{JOBS_ENDPOINT}/{id}");
    {
        let href = href.clone();
        scope.spawn(async move {
            let mut missing = 0u32;
            loop {
                match crate::api::get::<ConvertJob>(href.clone()).await {
                    Ok(j) => {
                        missing = 0;
                        let done = j.state.is_done();
                        job.try_set(Some(j));
                        err.try_set(None);
                        if done {
                            break;
                        }
                    }
                    Err(e) if is_gone(&e) => {
                        missing += 1;
                        if missing >= GONE_AFTER {
                            gone.try_set(true);
                            err.try_set(None);
                            break;
                        }
                    }
                    Err(e) => {
                        missing = 0;
                        err.try_set(Some(e.to_string()));
                    }
                }
                pause(JOB_PACE_MS).await;
            }
        });
    }
    let cancel = move |_| {
        let href = href.clone();
        scope.spawn(async move {
            let sent = gloo_net::http::Request::delete(&href).send().await;
            match sent {
                Ok(r) if r.ok() => toasts.ok("job cancelled"),
                Ok(r) => toasts.err(format!("cancel failed: HTTP {}", r.status())),
                Err(e) => toasts.err(format!("cancel failed: {e}")),
            }
        });
    };
    let running =
        move || !gone.get() && job.with(|j| j.as_ref().is_none_or(|j| !j.state.is_done()));
    view! {
        <div class="doclab-job">
            <div class="doclab-job-head">
                <span class="chip info">
                    <i class="dot"></i>
                    {move || job.with(|j| j.as_ref().map_or_else(|| "job accepted".to_string(), job_line))}
                </span>
                <span class="mono-sm dim">{id.clone()}</span>
                <span class="spacer"></span>
                <button
                    class="btn danger sm"
                    title="Cancel the job: its programs are stopped and its converted text deleted"
                    disabled=move || !running()
                    on:click=cancel
                >
                    "Cancel job"
                </button>
            </div>
            {move || {
                job.with(|j| {
                    j.as_ref()
                        .and_then(|j| j.pool)
                        .map(|p| {
                            format!(
                                "Parallel tool runs: {} running, {} waiting of {} slots (every caller's)",
                                p.running, p.waiting, p.slots
                            )
                        })
                })
                .map(|s| view! { <div class="mono-sm dim">{s}</div> })
            }}
            {move || {
                gone.get()
                    .then(|| {
                        view! {
                            <div class="notice warn">
                                "The job is gone: the gateway no longer has it (it was cancelled, expired or the gateway restarted). Its result, if it ended first, is in the history."
                            </div>
                        }
                    })
            }}
            {move || err.get().map(|e| view! { <div class="notice warn">{format!("reading the job: {e}")}</div> })}
            <div class="doclab-job-files">
                {move || {
                    job.with(|j| {
                        j.as_ref()
                            .map(|j| {
                                j.documents
                                    .iter()
                                    .map(|f| {
                                        let st = f.state.as_str();
                                        let parts = if f.parts > 0 {
                                            format!("{} of {} parts", f.parts_done, f.parts)
                                        } else {
                                            String::new()
                                        };
                                        let errc = f.error.clone().unwrap_or_default();
                                        view! {
                                            <div class="doclab-job-file">
                                                <span class=words::status_chip(st)>{words::state(st)}</span>
                                                <span class="doclab-file-name">{f.name.clone()}</span>
                                                <span class="dim mono-sm">{fmt_bytes(f.bytes)}</span>
                                                <span class="dim mono-sm">{parts}</span>
                                                <span class="mono-sm">{errc}</span>
                                            </div>
                                        }
                                    })
                                    .collect_view()
                            })
                    })
                }}
            </div>
        </div>
    }
}
