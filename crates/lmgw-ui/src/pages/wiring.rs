//! Wiring — the signal path of every chat model, one aligned row each:
//! source → GGUF on disk → configuration → exposure → llama-server. Read-only
//! by design: every broken link renders as the *link to the page that fixes
//! it*, never as a mutation performed here.
//!
//! Colour vocabulary is the one the rest of the UI already uses (app.css):
//! red = broken, amber = waiting on you, green = running, accent blue =
//! interactive. Healthy cells are plain text and an idle model is neutral —
//! models start on demand, so "not running" is their normal state — which
//! leaves a real break as the only thing that pops.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use lmgw_api_types::{LocalChain, OrphanGguf, UpstreamChain, WiringView};

use crate::fmt::{age, hue_for, of};
use crate::live::{use_live, LiveBus};
use crate::url_state::use_query_signal;
use crate::widgets::{
    filter_words, CopyBtn, Facet, FacetSet, FilterBar, PageFrame, Popover, Section,
};

fn edit_href(id: i64) -> String {
    format!("/models/local/{id}")
}

/// Query-string value encoder (the orphan wire-up prefill carries a path).
fn q(value: &str) -> String {
    js_sys::encode_uri_component(value).into()
}

/// The page's one fetch, kept in signals so a Retry refetches in place.
#[derive(Clone, Copy)]
struct WiringData {
    view: RwSignal<Option<WiringView>>,
    error: RwSignal<Option<String>>,
}

impl WiringData {
    fn new() -> Self {
        let d = Self {
            view: RwSignal::new(None),
            error: RwSignal::new(None),
        };
        d.load();
        d
    }

    fn load(self) {
        spawn_local(async move {
            match crate::api::get::<WiringView>("/api/wiring").await {
                Ok(v) => {
                    self.view.set(Some(v));
                    self.error.set(None);
                }
                Err(e) => self.error.set(Some(e.to_string())),
            }
        });
    }
}

/// A model nothing can serve as it stands: its file is gone, its download
/// failed, or it is switched on but no client can name it (private, and no
/// alias routes to it). Being idle, disabled or behind on an update is not.
fn is_problem(c: &LocalChain) -> bool {
    !c.file_exists || c.hf_status == "failed" || unreachable(c)
}

fn unreachable(c: &LocalChain) -> bool {
    c.enabled && !c.public && c.aliases.is_empty()
}

fn local_matches(c: &LocalChain, words: &[String]) -> bool {
    let hay = format!(
        "{} {} {} {} {}",
        c.model_id,
        c.public_name,
        c.aliases.join(" "),
        c.gguf_path,
        c.hf_repo
    )
    .to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

fn local_facet(c: &LocalChain, facet: &str) -> bool {
    match facet {
        "problems" => is_problem(c),
        "private" => !c.public,
        "disabled" => !c.enabled,
        _ => true,
    }
}

fn orphan_matches(o: &OrphanGguf, words: &[String]) -> bool {
    let hay = format!("{} {} {}", o.gguf_path, o.role_guess, o.suggested_id).to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

fn upstream_matches(u: &UpstreamChain, words: &[String]) -> bool {
    let hay = format!(
        "{} {} {} {}",
        u.name,
        u.protocol,
        u.prefix,
        u.aliases.join(" ")
    )
    .to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// Only weights can be wired up as a model. An older gateway sends no role;
/// its files are offered as before.
fn is_weights(o: &OrphanGguf) -> bool {
    matches!(o.role_guess.as_str(), "weights" | "")
}

/// A companion's role as the page names it (one, several), and what it is
/// for.
fn role_label(role: &str) -> (&'static str, &'static str, &'static str) {
    match role {
        "mmproj" => (
            "projector",
            "projectors",
            "Vision projector (mmproj) — attach it to the model it belongs to in that model's editor",
        ),
        "drafter" => (
            "drafter",
            "drafters",
            "Speculative-decoding drafter — attach it as a model's draft model in its editor",
        ),
        "imatrix" => (
            "imatrix",
            "imatrix files",
            "Importance matrix — input to quantization, not a model anything loads",
        ),
        _ => ("other", "others", "Not a file a chat model loads"),
    }
}

/// A path whose directory gives way before its file name: "…/gemma-4-12B-
/// it-qat-UD-Q4_K_XL.gguf" says more than "unsloth/gemma-4-12B-it-q…". The
/// copy button leads, so no ellipsis can swallow it.
#[component]
fn PathCell(path: String) -> impl IntoView {
    let (dir, file) = match path.rsplit_once('/') {
        Some((d, f)) => (format!("{d}/"), f.to_string()),
        None => (String::new(), path.clone()),
    };
    view! {
        <span class="id-cell">
            <CopyBtn text=path title="Copy the path"/>
            <span class="pfx">{dir}</span>
            <span class="id-base">{file}</span>
        </span>
    }
}

/// What the table shows under the current filter, with every count the
/// headings need.
#[derive(Clone, PartialEq, Default)]
struct Shown {
    locals: Vec<LocalChain>,
    locals_total: usize,
    problems: usize,
    weights: Vec<OrphanGguf>,
    weights_total: usize,
    companions: Vec<OrphanGguf>,
    companions_total: usize,
    upstreams: Vec<UpstreamChain>,
    upstreams_total: usize,
}

#[component]
pub fn Wiring() -> impl IntoView {
    let live = use_live();
    let data = WiringData::new();
    let query = use_query_signal("q");
    let facet = use_query_signal("show");
    let narrowed = Signal::derive(move || {
        !query.with(|q| q.trim().is_empty()) || !facet.with(String::is_empty)
    });

    let shown = Memo::new(move |_| {
        let words = filter_words(&query.get());
        let f = facet.get();
        data.view.with(|v| {
            let Some(v) = v else {
                return Shown::default();
            };
            let (weights, companions): (Vec<&OrphanGguf>, Vec<&OrphanGguf>) =
                v.orphans.iter().partition(|o| is_weights(o));
            // A facet narrows the models; the files and upstreams are none of
            // its kinds, so they step aside rather than show as "0 of 7".
            let others = f.is_empty();
            Shown {
                locals: v
                    .locals
                    .iter()
                    .filter(|c| local_matches(c, &words) && local_facet(c, &f))
                    .cloned()
                    .collect(),
                locals_total: v.locals.len(),
                problems: v.locals.iter().filter(|c| is_problem(c)).count(),
                weights_total: weights.len(),
                weights: weights
                    .into_iter()
                    .filter(|o| others && orphan_matches(o, &words))
                    .cloned()
                    .collect(),
                companions_total: companions.len(),
                companions: companions
                    .into_iter()
                    .filter(|o| others && orphan_matches(o, &words))
                    .cloned()
                    .collect(),
                upstreams: v
                    .upstreams
                    .iter()
                    .filter(|u| others && upstream_matches(u, &words))
                    .cloned()
                    .collect(),
                upstreams_total: v.upstreams.len(),
            }
        })
    });

    let facets = Signal::derive(move || {
        data.view.with(|v| {
            let locals = v.as_ref().map(|v| v.locals.as_slice()).unwrap_or_default();
            [
                ("problems", "Problems"),
                ("private", "Private"),
                ("disabled", "Disabled"),
            ]
            .into_iter()
            .map(|(id, label)| Facet {
                id: id.into(),
                label: label.into(),
                count: locals.iter().filter(|c| local_facet(c, id)).count(),
            })
            .collect::<Vec<_>>()
        })
    });

    let base_v1 = Signal::derive(move || {
        data.view
            .with(|v| v.as_ref().map(|v| format!("{}/v1", v.base_url)))
    });

    view! {
        <PageFrame
            title="Wiring"
            sub="chat models, from download to container"
            actions=move || {
                move || {
                    base_v1
                        .get()
                        .map(|url| {
                            view! {
                                <span class="wire-gw" title="The gateway base URL clients point at">
                                    <span class="dim">"Gateway"</span>
                                    <code>{url.clone()}</code>
                                    <CopyBtn text=url title="Copy the gateway base URL"/>
                                </span>
                            }
                        })
                }
            }
            toolbar=move || {
                view! {
                    <FilterBar
                        query=query
                        placeholder="Filter by model, file, alias or upstream"
                        shown=Signal::derive(move || shown.with(|s| s.locals.len()))
                        total=Signal::derive(move || shown.with(|s| s.locals_total))
                        noun="models"
                        facets=FacetSet { items: facets, active: facet }
                    />
                }
            }
        >
            {move || {
                data.error
                    .get()
                    .map(|e| {
                        let stale = data.view.with(Option::is_some);
                        view! {
                            <div class="notice err row">
                                {if stale { "Refreshing the wiring failed: " } else { "Loading the wiring failed: " }}
                                {e}
                                <button class="btn ghost sm" on:click=move |_| data.load()>
                                    "Retry"
                                </button>
                            </div>
                        }
                    })
            }}
            <Show when=move || data.view.with(Option::is_none) && data.error.with(Option::is_none)>
                <div class="card dim">"Loading…"</div>
            </Show>
            <Show when=move || data.view.with(|v| v.as_ref().is_some_and(|v| v.models_dir_missing))>
                <div class="notice warn">
                    <b>"Models directory is not set"</b>
                    "Nothing can be scanned or served until it is — set it under "
                    <A href=super::settings::href("router.models_dir")>
                        "Settings → Runtimes → Chat"
                    </A>
                    "."
                </div>
            </Show>
            <Show when=move || data.view.with(Option::is_some)>
                <LocalsSection shown=shown narrowed=narrowed facet=facet live=live/>
                <OrphanSections shown=shown narrowed=narrowed/>
                <UpstreamsSection shown=shown narrowed=narrowed/>
            </Show>
        </PageFrame>
    }
}

#[component]
fn LocalsSection(
    shown: Memo<Shown>,
    narrowed: Signal<bool>,
    facet: RwSignal<String>,
    live: LiveBus,
) -> impl IntoView {
    let count = Signal::derive(move || shown.with(|s| of(s.locals.len(), s.locals_total)));
    let running = Memo::new(move |_| {
        live.runtime.with(|rt| {
            rt.as_deref().map(|rt| {
                rt.iter()
                    .filter(|r| r.class == "chat" && r.state != "stopping")
                    .count()
            })
        })
    });
    let actions = move || {
        view! {
            {move || {
                let n = shown.with(|s| s.problems);
                (n > 0)
                    .then(|| {
                        let on = facet.with(|f| f == "problems");
                        view! {
                            <button
                                type="button"
                                class="count bad wire-problems"
                                aria-pressed=on.to_string()
                                title=if on { "Show every model again" } else { "Show only the models with a problem" }
                                on:click=move |_| {
                                    facet.set(if on { String::new() } else { "problems".into() })
                                }
                            >
                                {format!("{n} problem{}", if n == 1 { "" } else { "s" })}
                            </button>
                        }
                    })
            }}
            {move || match running.get() {
                Some(n) if n > 0 => view! {
                    <span class="chip ok">
                        <span class="dot"></span>
                        {format!("{n} running")}
                    </span>
                }
                .into_any(),
                Some(_) => view! {
                    <span class="chip off" title="Each model's container starts with its first request">
                        <span class="dot"></span>
                        "none running · on demand"
                    </span>
                }
                .into_any(),
                None => ().into_any(),
            }}
        }
    };
    view! {
        <Section
            title="Local models"
            count=count
            persist="wiring.locals"
            force_open=narrowed
            actions=actions
        >
            {move || {
                shown
                    .with(|s| s.locals_total == 0)
                    .then(|| {
                        view! {
                            <div class="card empty">
                                "No local models yet — add one from "
                                <A href="/models">"Models"</A>
                                " (Hugging Face wizard, or from a GGUF already on disk)."
                            </div>
                        }
                    })
            }}
            <Show when=move || shown.with(|s| s.locals_total > 0)>
                <div class="card pad0">
                    <table class="data wire-table">
                        <thead>
                            <tr>
                                <th>"Model"</th>
                                <th class="col-p2">"Source"</th>
                                <th>"GGUF"</th>
                                <th class="col-p2">"Config"</th>
                                <th>"Exposed as"</th>
                                <th>"Runtime"</th>
                            </tr>
                        </thead>
                        <tbody>
                            <Show when=move || shown.with(|s| s.locals.is_empty())>
                                <tr>
                                    <td colspan="6" class="dim">"No model matches the filter."</td>
                                </tr>
                            </Show>
                            <For
                                each=move || shown.with(|s| s.locals.clone())
                                key=|c| format!("{c:?}")
                                let:c
                            >
                                <LocalRow c=c live=live/>
                            </For>
                        </tbody>
                    </table>
                </div>
            </Show>
        </Section>
    }
}

#[component]
fn LocalRow(c: LocalChain, live: LiveBus) -> impl IntoView {
    let edit = edit_href(c.id);
    let hue = hue_for(&c.model_id);
    let muted = !c.enabled;

    // 1 — where the weights came from. A tracked download that is not done
    // (or has an update waiting) is the one state that links to Downloads.
    let source = if c.hf_repo.is_empty() {
        view! { <td class="col-p2 dim" title="Found in the models dir, not downloaded by lmgw">"disk"</td> }
            .into_any()
    } else {
        let tip = format!("{} · {}", c.hf_repo, c.hf_status);
        match c.hf_status.as_str() {
            "done" => view! { <td class="col-p2 dim" title=tip>"Hugging Face"</td> }.into_any(),
            "failed" => view! {
                <td class="col-p2 t-err" title=tip>
                    <A href="/downloads">"download failed"</A>
                </td>
            }
            .into_any(),
            other => {
                let label = if other == "update_available" {
                    "update available".to_string()
                } else {
                    other.replace('_', " ")
                };
                view! {
                    <td class="col-p2 t-warn" title=tip>
                        <A href="/downloads">{label}</A>
                    </td>
                }
                .into_any()
            }
        }
    };

    // 2 — the file on disk. The copy button leads the cell, so the path's
    // ellipsis can never swallow it.
    let from = if c.hf_repo.is_empty() {
        String::new()
    } else {
        format!("\nfrom Hugging Face: {} ({})", c.hf_repo, c.hf_status)
    };
    let gguf = if c.file_exists {
        let tip = format!("{}{from}", c.gguf_path);
        view! {
            <td class="clip mono-sm dim" title=tip>
                <PathCell path=c.gguf_path.clone()/>
            </td>
        }
        .into_any()
    } else {
        let tip = format!("{} is not under the models dir{from}", c.gguf_path);
        view! {
            <td class="clip mono-sm t-err" title=tip>
                <A href=edit.clone()>"missing — fix path"</A>
                " "
                <span class="dim">{c.gguf_path.clone()}</span>
            </td>
        }
        .into_any()
    };

    // 3 — the configuration its container starts with. argv is rendered
    // fresh at every start (§3.6), so an enabled model's config is by
    // definition what its next start uses.
    let config = if c.enabled {
        view! { <td class="col-p2 dim">"enabled"</td> }.into_any()
    } else {
        view! {
            <td class="col-p2">
                <A href=edit.clone() attr:title="Not served; its editor switches it back on">
                    "disabled — enable"
                </A>
            </td>
        }
        .into_any()
    };

    // 4 — the name a client asks for. Aliases past the first are counted,
    // and listed in the tooltip.
    let names: Vec<String> = if c.public {
        std::iter::once(c.public_name.clone())
            .chain(c.aliases.iter().cloned())
            .collect()
    } else {
        c.aliases.clone()
    };
    let exposure = if let Some(first) = names.first().cloned() {
        let more = names.len() - 1;
        let tip = if c.public {
            names.join("\n")
        } else {
            format!(
                "private — reachable through its aliases only:\n{}",
                names.join("\n")
            )
        };
        view! {
            <td title=tip>
                <span class="wire-name">
                    <code>{first.clone()}</code>
                    <CopyBtn text=first title="Copy the name clients request"/>
                </span>
                {(more > 0).then(|| view! { <MoreNames names=names.clone()/> })}
            </td>
        }
        .into_any()
    } else if c.enabled {
        view! {
            <td class="t-warn" title="No client can name it: it is private and no alias routes to it">
                <A href=edit.clone()>"private — make public"</A>
            </td>
        }
        .into_any()
    } else {
        view! { <td class="dim">"private"</td> }.into_any()
    };

    // 5 — its container, from the runtime frame.
    let runtime = if c.enabled {
        let model_id = StoredValue::new(c.model_id.clone());
        let state = move || {
            live.runtime.with(|rt| {
                rt.as_deref().and_then(|rt| {
                    model_id.with_value(|m| {
                        rt.iter()
                            .find(|r| r.class == "chat" && &r.model_id == m)
                            .map(|r| (r.state.clone(), r.started_at_age_seconds))
                    })
                })
            })
        };
        view! {
            <td>
                {move || {
                    let (cls, label, tip) = match state() {
                        Some((s, secs)) if s == "ready" => {
                            ("chip ok", format!("running · {}", age(secs)), "Its container is up")
                        }
                        Some((s, _)) => ("chip live", s, "Its container is changing state"),
                        None => (
                            "chip off",
                            "idle · on demand".to_string(),
                            "Not running; its first request starts it",
                        ),
                    };
                    view! {
                        <span class=cls title=tip>
                            <span class="dot"></span>
                            {label}
                        </span>
                    }
                }}
            </td>
        }
        .into_any()
    } else {
        view! { <td class="dim" title="Disabled models are not served">"not served"</td> }
            .into_any()
    };

    view! {
        <tr class:muted=muted>
            <td>
                <a href=edit class="row-link" title="Open the editor">
                    <span class="model-chip" style=format!("--hue:{hue}")>
                        <i></i>
                        {c.model_id.clone()}
                    </span>
                </a>
            </td>
            {source}
            {gguf}
            {config}
            {exposure}
            {runtime}
        </tr>
    }
}

/// The models dir's GGUFs no model references, split by what they are:
/// weights can become a model, companions only ride along with one.
#[component]
fn OrphanSections(shown: Memo<Shown>, narrowed: Signal<bool>) -> impl IntoView {
    let weights_count =
        Signal::derive(move || shown.with(|s| of(s.weights.len(), s.weights_total)));
    let companions_count =
        Signal::derive(move || shown.with(|s| of(s.companions.len(), s.companions_total)));
    let companions_summary = Signal::derive(move || {
        shown.with(|s| {
            let mut by_role: Vec<(&str, usize)> = Vec::new();
            for o in &s.companions {
                match by_role.iter_mut().find(|(r, _)| *r == o.role_guess) {
                    Some((_, n)) => *n += 1,
                    None => by_role.push((&o.role_guess, 1)),
                }
            }
            by_role
                .into_iter()
                .map(|(r, n)| {
                    let (one, many, _) = role_label(r);
                    format!("{n} {}", if n == 1 { one } else { many })
                })
                .collect::<Vec<_>>()
                .join(" · ")
        })
    });
    // A section with nothing in it has nothing to say; while filtering, one
    // with no match steps aside too.
    let show_weights = move || shown.with(|s| !s.weights.is_empty());
    let show_companions = move || shown.with(|s| !s.companions.is_empty());
    view! {
        <Show when=show_weights>
            <Section
                title="Unwired weights"
                count=weights_count
                summary=Signal::derive(|| "model files in the models dir no model uses".to_string())
                persist="wiring.unwired"
                force_open=narrowed
            >
                <div class="card pad0">
                    <table class="data wire-table">
                        <thead>
                            <tr>
                                <th>"File"</th>
                                <th class="col-p2">"Proposed id"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            <For
                                each=move || shown.with(|s| s.weights.clone())
                                key=|o| o.gguf_path.clone()
                                let:o
                            >
                                <WeightsRow o=o/>
                            </For>
                        </tbody>
                    </table>
                </div>
            </Section>
        </Show>
        <Show when=show_companions>
            <Section
                title="Unreferenced companion files"
                count=companions_count
                summary=companions_summary
                persist="wiring.companions"
                default_open=false
                force_open=narrowed
            >
                <div class="card pad0">
                    <table class="data wire-table">
                        <thead>
                            <tr>
                                <th>"File"</th>
                                <th>"Role"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            <For
                                each=move || shown.with(|s| s.companions.clone())
                                key=|o| o.gguf_path.clone()
                                let:o
                            >
                                <CompanionRow o=o/>
                            </For>
                        </tbody>
                    </table>
                </div>
            </Section>
        </Show>
    }
}

#[component]
fn WeightsRow(o: OrphanGguf) -> impl IntoView {
    // The create form reads both from the query string (see local_edit.rs).
    let wire_up = format!(
        "/models/local/new?gguf={}&model_id={}",
        q(&o.gguf_path),
        q(&o.suggested_id)
    );
    let title = format!("Create a chat model from this file, as {}", o.suggested_id);
    view! {
        <tr>
            <td class="clip mono-sm" title=o.gguf_path.clone()>
                <PathCell path=o.gguf_path.clone()/>
            </td>
            <td class="col-p2 mono-sm dim" title="The model id the create form proposes">
                {o.suggested_id.clone()}
            </td>
            <td class="actions">
                <a class="btn sm" href=wire_up title=title>
                    "Wire up"
                </a>
            </td>
        </tr>
    }
}

#[component]
fn CompanionRow(o: OrphanGguf) -> impl IntoView {
    let (label, _, tip) = role_label(&o.role_guess);
    // "drafter" is a guess from the file name, and a full model can carry the
    // same marker (…-MTP-IQ4_XS, weights with their own MTP layers): the way
    // to wire it up as a model stays one click away (par:PAR-9). A projector
    // or an imatrix is never a model.
    let wire_up = (o.role_guess == "drafter").then(|| {
        let href = format!(
            "/models/local/new?gguf={}&model_id={}",
            q(&o.gguf_path),
            q(&o.suggested_id)
        );
        view! {
            <a
                class="btn ghost sm"
                href=href
                title="Named like a speculative drafter — if it is a full model, create a chat model from it"
            >
                "Wire up"
            </a>
        }
    });
    view! {
        <tr>
            <td class="clip mono-sm dim" title=o.gguf_path.clone()>
                <PathCell path=o.gguf_path.clone()/>
            </td>
            <td>
                <span class="type-badge" title=tip>
                    {label}
                </span>
            </td>
            <td class="actions">{wire_up}</td>
        </tr>
    }
}

/// The names past the first, each readable and copyable (par:PAR-11): "+2
/// aliases" opens a list of every name a client can ask for, with a copy
/// button on each.
#[component]
fn MoreNames(names: Vec<String>) -> impl IntoView {
    let open = RwSignal::new(false);
    let btn: NodeRef<leptos::html::Button> = NodeRef::new();
    let more = names.len().saturating_sub(1);
    let names = StoredValue::new(names);
    view! {
        <button
            type="button"
            class="count names-more"
            node_ref=btn
            aria-haspopup="true"
            aria-expanded=move || open.get().to_string()
            title="Every name, each with a copy button"
            on:click=move |_| open.update(|o| *o = !*o)
        >
            {format!("+{} ▾", crate::fmt::count_of(more, "aliases"))}
        </button>
        <Popover open=open anchor=btn class="names-pop" min_width=240>
            <div class="pop-count names-count">
                {crate::fmt::count_of(names.with_value(Vec::len), "names")}
            </div>
            <div class="pop-list">
                {names
                    .get_value()
                    .into_iter()
                    .map(|n| {
                        view! {
                            <span class="wire-name names-item">
                                <code>{n.clone()}</code>
                                <CopyBtn text=n title="Copy this name"/>
                            </span>
                        }
                    })
                    .collect_view()}
            </div>
        </Popover>
    }
}

#[component]
fn UpstreamsSection(shown: Memo<Shown>, narrowed: Signal<bool>) -> impl IntoView {
    let count = Signal::derive(move || shown.with(|s| of(s.upstreams.len(), s.upstreams_total)));
    let empty = move || shown.with(|s| s.upstreams_total == 0);
    // Filtered to nothing: the section steps aside like the orphan ones.
    let hidden = move || narrowed.get() && shown.with(|s| s.upstreams.is_empty());
    view! {
        <Show when=move || !hidden()>
            <Section
                title="Remote upstreams"
                count=count
                persist="wiring.upstreams"
                force_open=narrowed
            >
                <Show when=empty>
                    <div class="card empty">
                        "No remote upstreams — add one under "
                        <A href="/upstreams">"Upstreams"</A>
                        "."
                    </div>
                </Show>
                <Show when=move || !empty()>
                    <div class="card pad0">
                        <table class="data wire-table">
                            <thead>
                                <tr>
                                    <th>"Upstream"</th>
                                    <th>"Protocol"</th>
                                    <th>"Exposed as"</th>
                                    <th>"State"</th>
                                </tr>
                            </thead>
                            <tbody>
                                <For
                                    each=move || shown.with(|s| s.upstreams.clone())
                                    key=|u| format!("{u:?}")
                                    let:u
                                >
                                    <UpstreamRow u=u/>
                                </For>
                            </tbody>
                        </table>
                    </div>
                </Show>
            </Section>
        </Show>
    }
}

#[component]
fn UpstreamRow(u: UpstreamChain) -> impl IntoView {
    let hue = hue_for(&u.name);
    let exposure = if u.expose_all {
        // The pattern is what a client may ask for; the prefix is what you
        // actually paste in front of an upstream model name.
        let (pattern, copy) = if u.prefix.is_empty() {
            ("*".to_string(), String::new())
        } else {
            (format!("{}/*", u.prefix), format!("{}/", u.prefix))
        };
        view! {
            <td class="clip" title="Passthrough: every model of this upstream">
                <span class="wire-name">
                    <code>{pattern}</code>
                    <CopyBtn text=copy title="Copy the request prefix"/>
                </span>
                <span class="dim">" every model"</span>
            </td>
        }
        .into_any()
    } else if let Some(first) = u.aliases.first().cloned() {
        let more = u.aliases.len() - 1;
        let names = u.aliases.clone();
        view! {
            <td class="clip" title=u.aliases.join("\n")>
                <span class="wire-name">
                    <code>{first.clone()}</code>
                    <CopyBtn text=first title="Copy the alias"/>
                </span>
                {(more > 0).then(|| view! { <MoreNames names=names/> })}
            </td>
        }
        .into_any()
    } else {
        view! {
            <td class="clip t-warn" title="No alias routes here and passthrough is off">
                <A href="/upstreams">"nothing exposed — enable passthrough"</A>
            </td>
        }
        .into_any()
    };
    view! {
        <tr class:muted=!u.enabled>
            <td>
                <a href="/upstreams" class="row-link" title="Open Upstreams">
                    <span class="model-chip" style=format!("--hue:{hue}")>
                        <i></i>
                        {u.name.clone()}
                    </span>
                </a>
            </td>
            <td>
                <span class="type-badge">{u.protocol.clone()}</span>
            </td>
            {exposure}
            <td>
                {if u.enabled {
                    view! { <span class="dim">"enabled"</span> }.into_any()
                } else {
                    view! {
                        <span class="chip off">
                            <span class="dot"></span>
                            "disabled"
                        </span>
                    }
                        .into_any()
                }}
            </td>
        </tr>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(f: impl FnOnce(&mut LocalChain)) -> LocalChain {
        let mut c = LocalChain {
            model_id: "m".into(),
            public_name: "m".into(),
            gguf_path: "acme/m.gguf".into(),
            file_exists: true,
            enabled: true,
            public: true,
            ..Default::default()
        };
        f(&mut c);
        c
    }

    #[test]
    fn only_what_breaks_serving_is_a_problem() {
        assert!(!is_problem(&chain(|_| {})));
        assert!(is_problem(&chain(|c| c.file_exists = false)));
        assert!(is_problem(&chain(|c| c.hf_status = "failed".into())));
        // Switched on, but nothing names it.
        assert!(is_problem(&chain(|c| c.public = false)));
        // …an alias does, and a disabled model is not asked for at all.
        assert!(!is_problem(&chain(|c| {
            c.public = false;
            c.aliases = vec!["a".into()];
        })));
        assert!(!is_problem(&chain(|c| {
            c.public = false;
            c.enabled = false;
        })));
        // Waiting on an update is not broken.
        assert!(!is_problem(&chain(
            |c| c.hf_status = "update_available".into()
        )));
    }

    #[test]
    fn a_projector_is_not_offered_as_a_model() {
        let o = |role: &str| OrphanGguf {
            role_guess: role.into(),
            ..Default::default()
        };
        assert!(is_weights(&o("weights")));
        assert!(is_weights(&o("")), "an older gateway's files stay offered");
        for role in ["mmproj", "drafter", "imatrix"] {
            assert!(!is_weights(&o(role)), "{role}");
        }
    }
}
