//! Overview — gateway state at a glance. Proving ground for the live-data
//! patterns: initial fetch via `/api/status` + `/api/connect` + `/api/logs`,
//! then everything moves on the shared SSE bus.
//!
//! The glance is what is running and what just happened, so those two own the
//! screen: the backends that are up, and the live feed filling whatever height
//! is left. Everything that is merely *configured* (the stopped models, how to
//! connect) folds to one line with its count.

use std::collections::BTreeMap;

use leptos::prelude::*;
use lmgw_api_types::{ConnectInfo, GatewayStatus, LogsResponse, ModelsFull, RequestRow};

use crate::fmt::{age, grouped, hue_for, log_time};
use crate::live::use_live;
use crate::model_ops::{LogsButton, ModelRunButtons};
use crate::widgets::{filter_words, matches_word, CopyBtn, Facet, FacetSet, FilterBar, PageFrame};
use crate::widgets::{PageMode, Section};

const FEED_LIMIT: usize = 25;

#[component]
pub fn Overview() -> impl IntoView {
    let live = use_live();

    let status = LocalResource::new(|| crate::api::get::<GatewayStatus>("/api/status"));
    let connect = LocalResource::new(|| crate::api::get::<ConnectInfo>("/api/connect"));
    // Enabled models of every class, to find the ones with no runtime entry
    // (§8: "one row per enabled-but-not-running model, with a Start action").
    let models = LocalResource::new(|| crate::api::get::<ModelsFull>("/api/models/full"));

    // Live feed: seeded from the log store, prepended from SSE.
    let feed = RwSignal::new(Vec::<RequestRow>::new());
    let seeded = RwSignal::new(false);
    let seed = LocalResource::new(move || {
        crate::api::get::<LogsResponse>(format!("/api/logs?limit={FEED_LIMIT}"))
    });
    Effect::new(move |_| {
        if seeded.get_untracked() {
            return;
        }
        if let Some(Ok(resp)) = seed.get() {
            feed.set(resp.logs);
            seeded.set(true);
        }
    });
    Effect::new(move |_| {
        let Some(row) = live.request.get() else {
            return;
        };
        feed.update(|f| {
            if f.first().map(|r| r.log_id) == Some(row.log_id) {
                return; // duplicate frame (reconnect replay)
            }
            f.insert(0, row);
            f.truncate(FEED_LIMIT);
        });
    });

    // Stat tiles: live stats once the bus delivers, else the initial fetch.
    let nums = Memo::new(move |_| {
        if let Some(s) = live.stats.get() {
            (
                s.active_requests,
                s.req_last_minute,
                s.err_last_minute,
                s.prompt_tokens + s.completion_tokens,
            )
        } else if let Some(Ok(s)) = status.get() {
            (
                s.requests.active,
                s.requests.last_minute,
                s.requests.errors_last_minute,
                s.requests.prompt_tokens + s.requests.completion_tokens,
            )
        } else {
            (0, 0, 0, 0)
        }
    });
    let uptime = move || {
        status
            .get()
            .and_then(|r| r.ok())
            .map(|s| age(s.uptime_seconds))
    };

    let sub = move || {
        status
            .get()
            .and_then(|r| r.ok())
            .map(|s| format!("v{} · up {}", s.version, age(s.uptime_seconds)))
            .unwrap_or_default()
    };

    // A fetch that failed says so where its answer would have been, with a
    // Retry: never "No models configured yet" or "No requests yet" about a
    // list that did not arrive (review code:U2).
    let models_err = Signal::derive(move || match models.get() {
        Some(Err(e)) => Some(e.to_string()),
        _ => None,
    });
    let models_ready = Signal::derive(move || matches!(models.get(), Some(Ok(_))));
    let retry_models = Callback::new(move |()| models.refetch());

    let rows = model_rows(live, models);
    let running = Memo::new(move |_| {
        rows.with(|r| {
            r.iter()
                .filter(|m| m.rt.is_some())
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let stopped = Memo::new(move |_| {
        rows.with(|r| {
            r.iter()
                .filter(|m| m.rt.is_none())
                .cloned()
                .collect::<Vec<_>>()
        })
    });

    view! {
        <PageFrame title="Overview" sub=sub mode=PageMode::Fill class="overview">

            {move || match status.get() {
                Some(Err(e)) if live.stats.with(Option::is_none) => {
                    Some(
                        view! {
                            <div class="chart-err row">
                                "Could not load the gateway status — " {e.to_string()}
                                <button class="btn ghost sm" on:click=move |_| status.refetch()>
                                    "Retry"
                                </button>
                            </div>
                        },
                    )
                }
                _ => None,
            }}
            <div class="auto-grid fit tile-row">
                <div class={move || if nums.get().0 > 0 { "tile hot" } else { "tile" }}>
                    <div class="tile-value">{move || nums.get().0.to_string()}</div>
                    <div class="tile-label">"active now"</div>
                </div>
                <div class="tile">
                    <div class="tile-value">{move || nums.get().1.to_string()}</div>
                    <div class="tile-label">"req / min"</div>
                </div>
                <div class={move || if nums.get().2 > 0 { "tile bad" } else { "tile" }}>
                    <div class="tile-value">{move || nums.get().2.to_string()}</div>
                    <div class="tile-label">"errors / min"</div>
                </div>
                // Counted in memory by this gateway process, so it starts over
                // with it: the tile says so, the tooltip says since when.
                <div
                    class="tile"
                    title=move || {
                        uptime()
                            .map(|u| {
                                format!(
                                    "prompt + completion tokens since the gateway started, {u} ago — Usage has the history",
                                )
                            })
                            .unwrap_or_default()
                    }
                >
                    <div class="tile-value">{move || grouped(nums.get().3)}</div>
                    <div class="tile-label">"tokens since start"</div>
                </div>
            </div>

            <div class="ov-split">
                <div class="ov-state">
                    <Section
                        title="Backends"
                        count=Signal::derive(move || grouped(running.with(Vec::len) as u64))
                        persist="overview.backends"
                    >
                        <RunningTable
                            rows=running
                            stopped=stopped
                            ready=models_ready
                            err=models_err
                            retry=retry_models
                        />
                    </Section>
                    <Section
                        title="Stopped"
                        count=Signal::derive(move || grouped(stopped.with(Vec::len) as u64))
                        summary=Signal::derive(move || stopped.with(|s| class_summary(s)))
                        persist="overview.stopped"
                        default_open=false
                    >
                        <StoppedTable rows=stopped ready=models_ready err=models_err retry=retry_models/>
                    </Section>
                    <Section
                        title="Connect"
                        summary=Signal::derive(move || connect_summary(connect.get()))
                        persist="overview.connect"
                        default_open=false
                    >
                        <ConnectPanel connect=connect/>
                    </Section>
                </div>

                <section class="ov-feed">
                    // The feed keeps the newest FEED_LIMIT and drops the rest as
                    // new ones arrive; the head says so, and where the rest are.
                    <div class="sec-head">
                        <span class="sec-title ov-title">"Live requests"</span>
                        <span class="sec-summary">
                            {format!("Latest {FEED_LIMIT} · live · ")}
                            <a href="/traffic">"Open in Traffic →"</a>
                        </span>
                    </div>
                    <div class="fill-pane card pad0">
                        <table class="data">
                            <thead>
                                <tr>
                                    <th title="local time">"Time"</th>
                                    <th>"Model"</th>
                                    <th class="col-p3">"Upstream"</th>
                                    <th>"Status"</th>
                                    <th class="num-h col-p2">"TTFB"</th>
                                    <th class="num-h">"Total"</th>
                                    <th class="num-h">"Tokens"</th>
                                </tr>
                            </thead>
                            <tbody>
                                <For each=move || feed.get() key=|r| r.log_id let:r>
                                    <FeedRow r=r/>
                                </For>
                            </tbody>
                        </table>
                        <Show when=move || feed.with(Vec::is_empty)>
                            {move || match seed.get() {
                                None => view! { <div class="empty dim">"Loading…"</div> }.into_any(),
                                Some(Err(e)) => {
                                    view! {
                                        <div class="chart-err row feed-err">
                                            "Could not load the latest requests — " {e.to_string()}
                                            " · new ones still appear here live"
                                            <button class="btn ghost sm" on:click=move |_| seed.refetch()>
                                                "Retry"
                                            </button>
                                        </div>
                                    }
                                        .into_any()
                                }
                                Some(Ok(_)) => {
                                    view! {
                                        <div class="empty">"No requests yet — traffic appears here live."</div>
                                    }
                                        .into_any()
                                }
                            }}
                        </Show>
                    </div>
                </section>
            </div>
        </PageFrame>
    }
}

/// One row of the runtime tables: either a live registry entry (running,
/// starting or stopping) or an enabled model the registry has no entry for
/// (not running — `rt: None`). Class-then-model_id order, same as the
/// Models page's per-class sections.
#[derive(Clone, PartialEq)]
struct ModelRow {
    class: &'static str,
    model_id: String,
    /// What clients ask for (`embed/…`, `audio/…`): the name, and the colour,
    /// every other page shows this model under (review ux:U-14). The id
    /// until the models list has it.
    name: String,
    warm_start: bool,
    rt: Option<lmgw_api_types::RuntimeStatus>,
}

fn class_lit(s: &str) -> &'static str {
    match s {
        "aux" => "aux",
        "audio" => "audio",
        "image" => "image",
        _ => "chat",
    }
}

fn class_rank(c: &str) -> u8 {
    match c {
        "chat" => 0,
        "aux" => 1,
        "image" => 3,
        _ => 2,
    }
}

fn class_title(c: &str) -> &'static str {
    match c {
        "chat" => "llama-server · chat",
        "aux" => "llama-server · embeddings + rerank",
        "image" => "stable-diffusion.cpp",
        _ => "audio.cpp",
    }
}

/// "chat 23 · aux 8 · audio 4 · image 2", in the class order.
fn class_counts(rows: &[ModelRow]) -> Vec<(&'static str, usize)> {
    let mut by: BTreeMap<(u8, &'static str), usize> = BTreeMap::new();
    for r in rows {
        *by.entry((class_rank(r.class), r.class)).or_default() += 1;
    }
    by.into_iter().map(|((_, c), n)| (c, n)).collect()
}

fn class_summary(rows: &[ModelRow]) -> String {
    class_counts(rows)
        .into_iter()
        .map(|(c, n)| format!("{c} {n}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Every model the page knows about: the registry's entries, plus each
/// enabled model the registry has none for.
fn model_rows(
    live: crate::live::LiveBus,
    models: LocalResource<crate::api::Result<ModelsFull>>,
) -> Memo<Vec<ModelRow>> {
    Memo::new(move |_| -> Vec<ModelRow> {
        let rt = live.runtime.get().unwrap_or_default();
        let mf = models.get().and_then(Result::ok);
        // (class, model id) → the exposed name, from the models list.
        let mut names: BTreeMap<(&'static str, String), String> = BTreeMap::new();
        if let Some(mf) = &mf {
            for l in &mf.local {
                names.insert(("chat", l.model.model_id.clone()), l.public_name.clone());
            }
            for a in &mf.aux {
                names.insert(("aux", a.model.model_id.clone()), a.public_name.clone());
            }
            for a in &mf.audio {
                names.insert(("audio", a.model.model_id.clone()), a.public_name.clone());
            }
            for i in &mf.image {
                names.insert(("image", i.model.model_id.clone()), i.public_name.clone());
            }
        }
        let name_of = |class: &'static str, id: &str| {
            names
                .get(&(class, id.to_string()))
                .filter(|n| !n.is_empty())
                .cloned()
                .unwrap_or_else(|| id.to_string())
        };
        let mut out: Vec<ModelRow> = rt
            .iter()
            .map(|r| {
                let class = class_lit(&r.class);
                ModelRow {
                    class,
                    model_id: r.model_id.clone(),
                    name: name_of(class, &r.model_id),
                    warm_start: false,
                    rt: Some(r.clone()),
                }
            })
            .collect();
        if let Some(mf) = &mf {
            let has_rt =
                |class: &str, mid: &str| rt.iter().any(|r| r.class == class && r.model_id == mid);
            let mut push = |class: &'static str, id: &str, enabled: bool, warm: bool| {
                if enabled && !has_rt(class, id) {
                    out.push(ModelRow {
                        class,
                        model_id: id.to_string(),
                        name: name_of(class, id),
                        warm_start: warm,
                        rt: None,
                    });
                }
            };
            for l in &mf.local {
                push(
                    "chat",
                    &l.model.model_id,
                    l.model.enabled,
                    l.model.warm_start,
                );
            }
            for a in &mf.aux {
                push(
                    "aux",
                    &a.model.model_id,
                    a.model.enabled,
                    a.model.warm_start,
                );
            }
            for a in &mf.audio {
                push(
                    "audio",
                    &a.model.model_id,
                    a.model.enabled,
                    a.model.warm_start,
                );
            }
            for i in &mf.image {
                push(
                    "image",
                    &i.model.model_id,
                    i.model.enabled,
                    i.model.warm_start,
                );
            }
        }
        out.sort_by(|a, b| {
            class_rank(a.class)
                .cmp(&class_rank(b.class))
                .then(a.name.cmp(&b.name))
        });
        out
    })
}

/// What is up right now: running, starting and stopping containers.
#[component]
fn RunningTable(
    rows: Memo<Vec<ModelRow>>,
    stopped: Memo<Vec<ModelRow>>,
    /// The models list is in: only then can "nothing configured" be said.
    ready: Signal<bool>,
    err: Signal<Option<String>>,
    retry: Callback<()>,
) -> impl IntoView {
    view! {
        <Show
            when=move || rows.with(|r| !r.is_empty())
            fallback=move || {
                move || {
                    if let Some(e) = err.get() {
                        return models_err_line(e, retry).into_any();
                    }
                    let line = if !ready.get() {
                        "Loading…".into_any()
                    } else if stopped.with(Vec::is_empty) {
                        view! {
                            "No models configured yet — add one from "
                            <a href="/models">"Models"</a>
                            "."
                        }
                            .into_any()
                    } else {
                        "Nothing is running. A local model starts when a request names it, or from Stopped below."
                            .into_any()
                    };
                    view! {
                        <div class="card pad0">
                            <div class="empty">{line}</div>
                        </div>
                    }
                        .into_any()
                }
            }
        >
            <div class="card pad0">
                <table class="data">
                    <thead>
                        <tr>
                            <th>"Class"</th>
                            <th>"Model"</th>
                            <th>"State"</th>
                            <th class="num-h col-p3">"Port"</th>
                            <th class="num-h col-p2">"In-flight"</th>
                            <th>"Uptime"</th>
                            <th class="col-p2">"Last used"</th>
                            <th></th>
                        </tr>
                    </thead>
                    <tbody>
                        <For
                            each=move || rows.get()
                            key=|r| {
                                (
                                    r.class,
                                    r.model_id.clone(),
                                    r.name.clone(),
                                    r.rt.as_ref().map(|rt| {
                                        (
                                            rt.state.clone(),
                                            rt.port,
                                            rt.in_flight,
                                            rt.started_at_age_seconds,
                                            rt.last_used_age_seconds,
                                        )
                                    }),
                                )
                            }
                            let:r
                        >
                            <RunningRow r=r/>
                        </For>
                    </tbody>
                </table>
            </div>
        </Show>
    }
}

#[component]
fn RunningRow(r: ModelRow) -> impl IntoView {
    let class = r.class;
    let model_id = r.model_id.clone();
    let state = r.rt.as_ref().map(|rt| rt.state.clone());
    let (chip_cls, chip_label) = match state.as_deref() {
        Some("ready") => ("chip ok", "ready"),
        Some("starting") => ("chip live", "starting"),
        Some("stopping") => ("chip live", "stopping"),
        _ => ("chip off", "stopped"),
    };
    let hue = hue_for(&r.name);
    // A narrow table drops Port, In-flight and Last used: the model's
    // tooltip carries them, so they stay a hover away (review par:PAR-6).
    let tip = {
        let mut parts = vec![if r.name == r.model_id {
            r.name.clone()
        } else {
            format!("{} · model id {}", r.name, r.model_id)
        }];
        if let Some(rt) = r.rt.as_ref() {
            if rt.port != 0 {
                parts.push(format!("port {}", rt.port));
            }
            parts.push(format!("{} in flight", rt.in_flight));
            parts.push(format!("up {}", age(rt.started_at_age_seconds)));
            parts.push(format!("last used {} ago", age(rt.last_used_age_seconds)));
            if let Some(p) = rt.llama_props.as_ref() {
                parts.push(llama_props_line(p));
            }
        }
        parts.join("\n")
    };
    // What a llama-server said about itself (llama egress design §4.2), or
    // why it could not: a start's warnings are this container's own.
    let warnings =
        r.rt.as_ref()
            .map(|rt| rt.warnings.clone())
            .unwrap_or_default();
    // Direct-endpoint link: llama-server (chat/aux) serves its own web UI on
    // its published port, and so does sd-server (image-generation §2.2);
    // audio.cpp does not (§8: "mark audio rows link-less"). BackendInfo (the
    // pre-per-model-containers host/port lookup) is gone (WP7) — derive the
    // host the same way it did: the browser's own hostname, since the
    // container publishes on all interfaces (§3.5).
    let link =
        r.rt.as_ref()
            .filter(|rt| rt.state == "ready" && class != "audio")
            .map(|rt| {
                let host = window()
                    .location()
                    .hostname()
                    .unwrap_or_else(|_| "127.0.0.1".into());
                format!("http://{host}:{}/", rt.port)
            });
    view! {
        <tr>
            <td>
                <span class="type-badge" title=class_title(class)>
                    {class}
                </span>
            </td>
            <td title=tip>
                <span class="model-chip" style=format!("--hue:{hue}")>
                    <i></i>
                    {r.name.clone()}
                </span>
            </td>
            <td>
                <span class=chip_cls>
                    <span class="dot"></span>
                    {chip_label}
                </span>
                // Ladder design §6: the rung this container runs (gguf file
                // name a hover away) and, while a climb is in progress, its
                // target and reason. Absent on a row without a ladder.
                {r.rt
                    .as_ref()
                    .and_then(|rt| rt.rung.clone())
                    .map(|rung| {
                        view! {
                            <span class="type-badge" title=rung.gguf>
                                {format!("rung {}/{}", rung.rung, rung.of)}
                            </span>
                        }
                    })}
                {r.rt
                    .as_ref()
                    .and_then(|rt| rt.llama_props.as_ref())
                    .map(|p| {
                        view! {
                            <span class="type-badge" title=llama_props_line(p)>
                                {llama_props_badge(p)}
                            </span>
                        }
                    })}
                {(!warnings.is_empty())
                    .then(|| {
                        view! {
                            <span class="chip warn" title=warnings.join("\n")>
                                {format!("{} warning{}", warnings.len(), if warnings.len() == 1 { "" } else { "s" })}
                            </span>
                        }
                    })}
                {r.rt
                    .as_ref()
                    .and_then(|rt| rt.climbing.clone())
                    .map(|c| {
                        view! {
                            <span class="type-badge climbing-badge" title=c.reason>
                                {format!("climbing to {}/{}", c.to, c.of)}
                            </span>
                        }
                    })}
                // Ownership (candidate-aliases design §4.4, §6): absent is
                // the owner's — the normal case, nothing to say about it.
                {r.rt
                    .as_ref()
                    .filter(|rt| rt.owner.as_deref() == Some("background"))
                    .map(|_| {
                        view! {
                            <span
                                class="type-badge"
                                title="Started by a background candidate alias request; the owner has not claimed it since"
                            >
                                "background"
                            </span>
                        }
                    })}
                {r.rt
                    .as_ref()
                    .filter(|rt| rt.draining_for_owner)
                    .map(|_| {
                        view! {
                            <span
                                class="chip warn"
                                title="An owner request is waiting for room on this GPU; background traffic skips this model until it is idle"
                            >
                                "draining for owner"
                            </span>
                        }
                    })}
            </td>
            <td class="num col-p3">
                {r.rt.as_ref().filter(|rt| rt.port != 0).map(|rt| rt.port.to_string()).unwrap_or_default()}
            </td>
            <td class="num col-p2">
                {r.rt.as_ref().map(|rt| rt.in_flight.to_string()).unwrap_or_default()}
            </td>
            <td class="dim mono-sm">
                {r.rt.as_ref().map(|rt| age(rt.started_at_age_seconds)).unwrap_or_default()}
            </td>
            <td class="dim mono-sm col-p2">
                {r.rt.as_ref().map(|rt| age(rt.last_used_age_seconds)).unwrap_or_default()}
            </td>
            <td class="actions">
                <div class="row-acts">
                    // An action, so it never folds away with the columns.
                    {link
                        .map(|url| {
                            view! {
                                <a href=url target="_blank" rel="noreferrer" title="The backend's own web UI, on its published port">
                                    "web UI ↗"
                                </a>
                            }
                        })}
                    <LogsButton class=class model_id=model_id.clone()/>
                    <ModelRunButtons class=class model_id=model_id running=state/>
                </div>
            </td>
        </tr>
    }
}

/// What a llama-server said about itself in `GET /props`, in one line: its
/// build, one slot's context and what it takes in besides text. "unknown"
/// where it did not say — never a guess.
pub(crate) fn llama_props_line(p: &lmgw_api_types::LlamaProps) -> String {
    let yes_no = |v: Option<bool>| match v {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    };
    format!(
        "llama-server {} · slot context {} · vision {}, audio {}, video {}",
        p.build_info.as_deref().unwrap_or("build unknown"),
        p.n_ctx_slot
            .map_or("unknown".to_string(), |n| format!("{} tokens", grouped(n))),
        yes_no(p.vision),
        yes_no(p.audio),
        yes_no(p.video),
    )
}

/// [`llama_props_line`] as a badge: one slot's context, and the media the
/// server takes.
pub(crate) fn llama_props_badge(p: &lmgw_api_types::LlamaProps) -> String {
    let mut out = p
        .n_ctx_slot
        .map_or("ctx ?".to_string(), |n| format!("{} ctx", grouped(n)));
    for (name, on) in [("vision", p.vision), ("audio", p.audio), ("video", p.video)] {
        if on == Some(true) {
            out.push_str(" · ");
            out.push_str(name);
        }
    }
    out
}

/// The models list did not load: said where the model rows would be.
fn models_err_line(e: String, retry: Callback<()>) -> impl IntoView {
    view! {
        <div class="chart-err row">
            "Could not load the configured models — " {e}
            <button class="btn ghost sm" on:click=move |_| retry.run(())>
                "Retry"
            </button>
        </div>
    }
}

/// Every enabled model that is not up, with its Start — a facet per class,
/// because "is my embedder there" is a question about one class.
#[component]
fn StoppedTable(
    rows: Memo<Vec<ModelRow>>,
    ready: Signal<bool>,
    err: Signal<Option<String>>,
    retry: Callback<()>,
) -> impl IntoView {
    let query = RwSignal::new(String::new());
    let class = RwSignal::new(String::new());
    let facets = FacetSet {
        items: Signal::derive(move || {
            rows.with(|r| {
                class_counts(r)
                    .into_iter()
                    .map(|(c, n)| Facet {
                        id: c.to_string(),
                        label: c.to_string(),
                        count: n,
                    })
                    .collect()
            })
        }),
        active: class,
    };
    let shown = Memo::new(move |_| {
        let words = query.with(|q| filter_words(q));
        let want = class.get();
        rows.with(|r| {
            r.iter()
                .filter(|m| want.is_empty() || m.class == want)
                .filter(|m| {
                    words
                        .iter()
                        .all(|w| matches_word(&m.name, w) || matches_word(&m.model_id, w))
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    view! {
        <div class="ov-stopped">
            <FilterBar
                query=query
                placeholder="Filter stopped models"
                shown=Signal::derive(move || shown.with(Vec::len))
                total=Signal::derive(move || rows.with(Vec::len))
                noun="stopped models"
                facets=facets
            />
            <div class="card pad0">
                <table class="data">
                    <thead>
                        <tr>
                            <th>"Class"</th>
                            <th>"Model"</th>
                            <th></th>
                        </tr>
                    </thead>
                    <tbody>
                        <For
                            each=move || shown.get()
                            key=|r| (r.class, r.model_id.clone())
                            let:r
                        >
                            <StoppedRow r=r/>
                        </For>
                    </tbody>
                </table>
                <Show when=move || shown.with(Vec::is_empty)>
                    {move || {
                        if let Some(e) = err.get() {
                            return models_err_line(e, retry).into_any();
                        }
                        let line = if !ready.get() {
                            "Loading…"
                        } else if rows.with(Vec::is_empty) {
                            "Every enabled model is running."
                        } else {
                            "No stopped model matches."
                        };
                        view! { <div class="empty">{line}</div> }.into_any()
                    }}
                </Show>
            </div>
        </div>
    }
}

#[component]
fn StoppedRow(r: ModelRow) -> impl IntoView {
    let class = r.class;
    let hue = hue_for(&r.name);
    let model_id = r.model_id.clone();
    let tip = if r.name == r.model_id {
        r.name.clone()
    } else {
        format!("{} · model id {}", r.name, r.model_id)
    };
    view! {
        <tr>
            <td>
                <span class="type-badge" title=class_title(class)>
                    {class}
                </span>
            </td>
            <td class="clip" title=tip>
                <i class="swatch" style=format!("--hue:{hue}")></i>
                <span class="mono-sm">{r.name.clone()}</span>
                {r
                    .warm_start
                    .then(|| view! { <span class="dim" title="starts at app launch">" · warm start"</span> })}
            </td>
            <td class="actions">
                <div class="row-acts">
                    <LogsButton class=class model_id=model_id.clone()/>
                    <ModelRunButtons class=class model_id=model_id running=None/>
                </div>
            </td>
        </tr>
    }
}

/// "http://127.0.0.1:8787 · auth off · 38 models + kilo/…, aistudio/…".
fn connect_summary(ci: Option<crate::api::Result<ConnectInfo>>) -> String {
    match ci {
        None => "…".into(),
        Some(Err(e)) => format!("could not load — {e}"),
        Some(Ok(ci)) => {
            let mut s = ci
                .bases
                .first()
                .map(|b| b.url.clone())
                .unwrap_or_else(|| "no listener".into());
            s.push_str(if ci.auth_enabled {
                " · auth on"
            } else {
                " · auth off"
            });
            s.push_str(&format!(" · {} models", grouped(ci.models.len() as u64)));
            if !ci.catalog_prefixes.is_empty() {
                let p: Vec<String> = ci
                    .catalog_prefixes
                    .iter()
                    .map(|p| format!("{p}/…"))
                    .collect();
                s.push_str(&format!(" + {}", p.join(", ")));
            }
            s
        }
    }
}

#[component]
fn ConnectPanel(connect: LocalResource<crate::api::Result<ConnectInfo>>) -> impl IntoView {
    let catalog = crate::catalog::use_model_catalog();
    view! {
        <div class="card">
            {move || match connect.get() {
                None => view! { <span class="dim">"…"</span> }.into_any(),
                Some(Err(e)) => view! { <span class="dim">{e.to_string()}</span> }.into_any(),
                Some(Ok(ci)) => {
                    let hint = if ci.auth_enabled {
                        view! {
                            "auth on — use a gateway key from "
                            <a href="/usage/keys">"Usage → Keys"</a>
                        }
                            .into_any()
                    } else {
                        "auth off — any key value works".into_any()
                    };
                    let auth_enabled = ci.auth_enabled;
                    let bases = ci.bases;
                    let models = ci.models;
                    let prefixes = ci.catalog_prefixes;
                    // Snippets are copy-paste-ready: real base URL, and the
                    // first exposed model rather than a <model> placeholder.
                    let base = bases
                        .first()
                        .map(|b| b.url.clone())
                        .unwrap_or_else(|| "http://127.0.0.1:8787".into());
                    let model = models
                        .first()
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| "<model>".into());
                    // Filed by class the way the pickers file them (the catalog
                    // knows each name's runtime); an alias is its own group,
                    // whatever it routes to.
                    let groups = Memo::new(move |_| {
                        let entries = catalog.entries.get();
                        let mut by: BTreeMap<(usize, String), Vec<String>> = BTreeMap::new();
                        for m in &models {
                            let key = if m.source == "alias" {
                                (90, "Aliases".to_string())
                            } else {
                                entries
                                    .iter()
                                    .find(|e| e.id == m.name)
                                    .map(|e| (e.group_rank(), e.group_label.clone()))
                                    .unwrap_or((80, "Local".to_string()))
                            };
                            by.entry(key).or_default().push(m.name.clone());
                        }
                        by.into_iter().map(|((_, label), names)| (label, names)).collect::<Vec<_>>()
                    });
                    view! {
                        <div class="connect-bases">
                            <div class="mini-head">"Base URLs · " {hint}</div>
                            <For each=move || bases.clone() key=|b| b.url.clone() let:b>
                                <div class="copy-line">
                                    <span class="mono-sm">{b.url.clone()}</span>
                                    <span class="dim">{format!(" ({})", b.label)}</span>
                                    <CopyBtn text=format!("{}/v1", b.url)/>
                                </div>
                            </For>
                            <div class="dim mini-note">
                                "OpenAI clients: base + /v1 · Anthropic clients: base as-is"
                            </div>
                        </div>
                        <div class="auto-grid connect-groups">
                            <For
                                each=move || groups.get()
                                key=|(label, names)| (label.clone(), names.len())
                                let:g
                            >
                                <div class="connect-group">
                                    <div class="mini-head">
                                        {g.0.clone()}
                                        <span class="count">{grouped(g.1.len() as u64)}</span>
                                    </div>
                                    {g
                                        .1
                                        .iter()
                                        .map(|name| {
                                            view! {
                                                <div class="copy-line" title=name.clone()>
                                                    <i
                                                        class="swatch"
                                                        style=format!("--hue:{}", hue_for(name))
                                                    ></i>
                                                    <span class="mono-sm cg-name">{name.clone()}</span>
                                                    <CopyBtn text=name.clone()/>
                                                </div>
                                            }
                                        })
                                        .collect_view()}
                                </div>
                            </For>
                            <Show when={
                                let has = !prefixes.is_empty();
                                move || has
                            }>
                                <div class="connect-group">
                                    <div class="mini-head">
                                        "Passthrough catalogs"
                                        <span class="count">{grouped(prefixes.len() as u64)}</span>
                                    </div>
                                    {prefixes
                                        .iter()
                                        .map(|p| {
                                            view! {
                                                <div class="copy-line">
                                                    <span class="type-badge" title="passthrough catalog">
                                                        {format!("{p}/…")}
                                                    </span>
                                                    <a class="dim" href="/models/catalog">
                                                        "browse"
                                                    </a>
                                                </div>
                                            }
                                        })
                                        .collect_view()}
                                </div>
                            </Show>
                        </div>
                        <Snippets base=base model=model auth_enabled=auth_enabled/>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// Collapsible copy-paste block: the three ways to point a client at the
/// gateway (raw curl, OpenAI SDK, Claude Code env). Ported from the old
/// dashboard's `<details>snippets</details>`, with a copy button per snippet.
#[component]
fn Snippets(base: String, model: String, auth_enabled: bool) -> impl IntoView {
    // With auth off the gateway ignores the key but clients still insist on
    // sending one — same convention the panel states above.
    let key = if auth_enabled {
        "<gateway key>"
    } else {
        "any-value"
    };
    let curl = format!(
        "curl {base}/v1/chat/completions \\\n  \
         -H 'Authorization: Bearer {key}' -H 'Content-Type: application/json' \\\n  \
         -d '{{\"model\":\"{model}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}'"
    );
    let sdk = format!(
        "from openai import OpenAI\n\
         client = OpenAI(base_url=\"{base}/v1\", api_key=\"{key}\")"
    );
    let claude = format!("export ANTHROPIC_BASE_URL={base}\nexport ANTHROPIC_API_KEY={key}");
    view! {
        <details class="snippets">
            <summary>"snippets"</summary>
            <div class="dim mini-note">
                {if auth_enabled {
                    view! { "Bearer token: a gateway key from " <a href="/usage/keys">"Usage → Keys"</a> "." }
                        .into_any()
                } else {
                    "Auth is off — any key value works, clients just need one.".into_any()
                }}
            </div>
            <Snippet label="curl" code=curl/>
            <Snippet label="OpenAI SDK" code=sdk/>
            <Snippet label="Claude Code via the gateway" code=claude/>
        </details>
    }
}

#[component]
fn Snippet(label: &'static str, code: String) -> impl IntoView {
    view! {
        <div class="snippet">
            <div class="snippet-head">
                <span class="preview-lang">{label}</span>
                <CopyBtn text=code.clone() title="Copy snippet"/>
            </div>
            <pre class="preset">{code}</pre>
        </div>
    }
}

#[component]
fn FeedRow(r: RequestRow) -> impl IntoView {
    let t = log_time(&r.ts);
    let ok = r.status < 400;
    let tokens = match (r.prompt_tokens, r.completion_tokens) {
        (Some(p), Some(c)) => format!("{p} + {c}"),
        (Some(p), None) => p.to_string(),
        _ => "—".into(),
    };
    let name = if r.ingress_proto == "mcp" {
        r.mcp_tool
            .clone()
            .unwrap_or_else(|| r.requested_alias.clone())
    } else {
        r.requested_alias.clone()
    };
    let hue = hue_for(&name);
    // Upstream and TTFB fold away on a narrow feed: the row's tooltip keeps
    // them, the name's too (its own title would shadow the row's).
    let mut tip = t.utc.clone();
    if let Some(u) = r.upstream_name.as_deref().filter(|u| !u.is_empty()) {
        tip.push_str(&format!(" · {u}"));
    }
    if let Some(v) = r.ttfb_ms {
        tip.push_str(&format!(" · TTFB {v} ms"));
    }
    if let Some(m) = r.error_msg.as_deref() {
        tip.push_str(&format!(" · {m}"));
    }
    let name_tip = format!("{name}\n{tip}");
    view! {
        <tr class:err-row=!ok title=tip>
            <td class="dim mono-sm">{t.time}</td>
            <td class="clip" title=name_tip>
                <i class="swatch" style=format!("--hue:{hue}")></i>
                <span class="mono-sm">{name.clone()}</span>
            </td>
            <td class="dim col-p3">{r.upstream_name.clone().unwrap_or_default()}</td>
            <td>
                <span class=if ok { "status-ok" } else { "status-err" }>{r.status}</span>
                {r.streamed.then(|| view! { <span class="dim">" ~"</span> })}
            </td>
            <td class="num col-p2">{r.ttfb_ms.map(|v| format!("{v} ms")).unwrap_or_default()}</td>
            <td class="num">{r.total_ms.map(|v| format!("{v} ms")).unwrap_or_default()}</td>
            <td class="num">{tokens}</td>
        </tr>
    }
}
