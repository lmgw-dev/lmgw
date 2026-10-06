//! App chrome: the grouped sidebar navigation, which folds to an icon rail,
//! with the gateway's state at its foot: the traffic pulse and the GPU pill.
//! There is no title bar of the app's own: in the shell the desktop frames
//! the window (KWin's title bar), and a browser has its tab.

use leptos::html;
use leptos::prelude::*;
use leptos_router::components::A;

use crate::fmt::{age, human_bytes};
use crate::widgets::{use_toasts, Popover};

/// Below this window width the sidebar is a rail whatever the choice says:
/// 216px of labels would leave a 900px window a page too narrow for its own
/// tables. CSS px, so the interface scale is already in it.
const RAIL_BELOW: f64 = 1100.0;

#[derive(Clone, Copy)]
pub struct SidebarState {
    /// The owner's choice, `rail` or `full`, persisted as `lmgw.ui.sidebar`.
    pref: RwSignal<String>,
    /// The window is too narrow for the full sidebar.
    pub narrow: Memo<bool>,
    /// What is drawn: the choice, or forced by a narrow window.
    pub rail: Memo<bool>,
}

impl SidebarState {
    fn toggle(&self) {
        let rail = self.pref.get_untracked() == "rail";
        self.pref
            .set(if rail { "full" } else { "rail" }.to_string());
    }
}

fn inner_width() -> f64 {
    window()
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(1280.0)
}

/// Install the sidebar state. Called once from `App`, which draws the rail
/// class on the shell grid.
///
/// The window width is tracked here and nowhere else: the sidebar is part of
/// the window, while everything inside the content pane answers to the
/// pane's own width (container queries, [`crate::charts::use_element_size`]).
pub fn provide_sidebar() -> SidebarState {
    let (width, set_width) = signal(inner_width());
    // Lives for the life of the app, like the SSE bus.
    let _ = window_event_listener(leptos::ev::resize, move |_| {
        let w = inner_width();
        if w != width.get_untracked() {
            set_width.set(w);
        }
    });
    let pref = crate::prefs::persisted_string("sidebar", "full");
    let narrow = Memo::new(move |_| width.get() < RAIL_BELOW);
    let rail = Memo::new(move |_| pref.with(|p| p == "rail") || narrow.get());
    let sidebar = SidebarState { pref, narrow, rail };
    provide_context(sidebar);
    sidebar
}

/// Live gateway traffic, the app's signature: a wave that runs while requests
/// are in flight, how many, and the live output rate. Both numbers are always
/// shown, 0 included, like the fields of a status bar: a slot that empties
/// when the gateway goes quiet reads as a gap in the layout. On the rail only
/// the wave is drawn, and the words are its tooltip.
#[component]
fn Pulse() -> impl IntoView {
    let live = crate::live::use_live();
    let active = Memo::new(move |_| {
        live.stats
            .get()
            .map(|s| s.active_requests)
            .unwrap_or_default()
    });
    let pulse_class = move || {
        if active.get() > 0 {
            "pulse"
        } else {
            "pulse idle"
        }
    };
    let label = move || format!("{} active", active.get());
    // Live generation speed across every in-flight stream (telemetry's `tok_s`,
    // refreshed twice a second while active). Approximate — one SSE chunk is
    // one token only for llama.cpp — hence the "~".
    let tok_s = Memo::new(move |_| live.stats.get().map(|s| s.tok_s).unwrap_or_default());
    // Below 1 tok/s nothing is being generated that the gateway can measure:
    // no stream, a stream that just opened, or a non-streaming request, which
    // never reports a rate. That reads 0, in grey; a measured rate is amber.
    let streaming = Memo::new(move |_| active.get() > 0 && tok_s.get() >= 1.0);
    // Right-aligned in its own slot, so the wave and the count never move.
    let rate_num = move || {
        if streaming.get() {
            format!("~{}", tok_s.get().round() as u64)
        } else {
            "0".to_string()
        }
    };
    let title = move || {
        let rate = if streaming.get() {
            format!(
                ", ~{} output tokens/s (approximate)",
                tok_s.get().round() as u64
            )
        } else {
            String::new()
        };
        format!("Gateway traffic: {}{rate}", label())
    };
    view! {
        <div class=pulse_class title=title>
            <svg width="48" height="12" viewBox="0 0 64 16">
                <path
                    class="wave"
                    d="M0 8 h6 l3 -5 4 10 3 -7 2 2 h8 l3 -6 4 9 2 -3 h9 l2 -4 4 7 3 -3 h11"
                />
            </svg>
            <span class="label">{label}</span>
            <span class="rate" class:on=move || streaming.get()>
                <span class="n">{rate_num}</span>
                " tok/s"
            </span>
        </div>
    }
}

/// The GPU at a glance (UX plan N3): an always-present quiet "GPU" pill —
/// with how many local models are up — that opens what is on the card, what
/// held chat requests fall back to, and the hold switch. While the hold is
/// engaged it is the amber HOLD pill (gpu-hold design §6), fed by the same
/// `vram` frame as the settings page and the tray.
#[component]
fn GpuPill(open: RwSignal<bool>, anchor: NodeRef<html::Button>) -> impl IntoView {
    let live = crate::live::use_live();
    let held = Memo::new(move |_| {
        live.vram
            .with(|v| v.as_ref().is_some_and(|v| v.hold_active))
    });
    let draining = Memo::new(move |_| {
        live.vram
            .with(|v| v.as_ref().map(|v| v.draining.len()).unwrap_or(0))
    });
    let running = Memo::new(move |_| {
        live.runtime
            .with(|r| r.as_ref().map(|r| r.len()).unwrap_or(0))
    });
    // A benchmark run holds the card (benchmark design §3.2).
    let bench = Memo::new(move |_| {
        live.vram.with(|v| {
            v.as_ref()
                .and_then(|v| v.benchmark.as_ref().map(|b| b.run_id))
        })
    });
    let label = move || {
        if held.get() {
            match draining.get() {
                0 => "HOLD".to_string(),
                n => format!("HOLD · {n} draining"),
            }
        } else if let Some(run) = bench.get() {
            format!("BENCH · run {run}")
        } else {
            match running.get() {
                0 => "GPU".to_string(),
                n => format!("GPU · {n}"),
            }
        }
    };
    let title = move || {
        if held.get() {
            "GPU hold: local models are paused and new loads are refused or re-routed. Click for details and Release."
                .to_string()
        } else if let Some(run) = bench.get() {
            format!("Benchmark run {run} holds the GPU: local models answer from their fallback or are refused until it ends. Click for the run.")
        } else {
            match running.get() {
                0 => "GPU: no local model is running. Click for the hold switch.".to_string(),
                1 => "GPU: 1 local model is running. Click for details and the hold switch."
                    .to_string(),
                n => format!(
                    "GPU: {n} local models are running. Click for details and the hold switch."
                ),
            }
        }
    };
    view! {
        <button
            type="button"
            node_ref=anchor
            class=move || match (held.get(), bench.get().is_some(), running.get() > 0) {
                (true, _, _) => "hold-pill",
                (false, true, _) => "gpu-pill bench",
                (false, false, true) => "gpu-pill busy",
                (false, false, false) => "gpu-pill",
            }
            aria-expanded=move || open.get().to_string()
            title=title
            on:click=move |_| open.update(|o| *o = !*o)
        >
            // The hold pill shows no dot beside its word; on the rail, where
            // the word does not fit, the dot is all either pill shows.
            <span class="dot"></span>
            <span class="pill-text">{label}</span>
        </button>
    }
}

/// The pill's popover: the card, the local models on it, and the hold.
#[component]
fn GpuPanel(open: RwSignal<bool>) -> impl IntoView {
    let live = crate::live::use_live();
    let toasts = use_toasts();
    let busy = RwSignal::new(false);
    let vram = move || live.vram.get();
    let held = move || vram().is_some_and(|v| v.hold_active);
    let device = move || {
        vram().map(|v| {
            let name = v
                .devices
                .first()
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "GPU".to_string());
            let free = match (v.free_bytes, v.capacity_bytes) {
                (Some(f), c) if c > 0 => format!("{} free of {}", human_bytes(f), human_bytes(c)),
                (Some(f), _) => format!("{} free", human_bytes(f)),
                (None, _) => "free memory not measured".to_string(),
            };
            (name, free)
        })
    };
    // Every local container, whatever its class: they all share the card.
    // The ledger's estimate is added where it has one.
    let rows = move || {
        let est = |class: &str, model: &str| {
            live.vram.with(|v| {
                v.as_ref().and_then(|v| {
                    v.resident
                        .iter()
                        .find(|r| r.container == class && r.model == model)
                        .map(|r| r.estimated_bytes)
                })
            })
        };
        live.runtime
            .get()
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                let bytes = est(&r.class, &r.model_id);
                (r, bytes)
            })
            .collect::<Vec<_>>()
    };
    let fallback = move || vram().and_then(|v| v.hold_fallback_alias);
    let draining = move || vram().map(|v| v.draining).unwrap_or_default();
    // The two shares (candidate-aliases design §4.7, §12.26): what lmgw's own
    // containers hold versus everything else on the card — games, the
    // desktop, other apps. `None` on either is a dash, never 0: an
    // unmeasured share is not the same as an empty one.
    let dash_bytes = |b: Option<u64>| b.map(human_bytes).unwrap_or_else(|| "—".to_string());
    let shares = move || {
        vram()
            .map(|v| {
                (
                    dash_bytes(v.lmgw_share_bytes),
                    dash_bytes(v.outside_share_bytes),
                )
            })
            .unwrap_or_else(|| ("—".to_string(), "—".to_string()))
    };
    let trigger_armed = move || vram().is_some_and(|v| v.external_trigger_active);
    let trigger_label = move || match vram() {
        Some(v) if v.external_trigger_active => "External fallback: armed".to_string(),
        Some(v) => format!(
            "External fallback: off — {}",
            v.external_trigger_reason
                .unwrap_or_else(|| "not reported".to_string())
        ),
        None => "External fallback: not reported".to_string(),
    };
    let close = move |_| open.set(false);
    view! {
        <div class="pop-inner gpu-panel">
            <div class="gpu-row gpu-dev">
                {move || match device() {
                    Some((name, free)) => view! {
                        <b>{name}</b>
                        <span class="dim">{free}</span>
                    }
                    .into_any(),
                    None => view! { <span class="dim">"GPU status not reported yet"</span> }.into_any(),
                }}
            </div>
            <div class="gpu-row dim">
                {move || {
                    let (lmgw, outside) = shares();
                    format!("lmgw {lmgw} · outside {outside}")
                }}
            </div>
            <div class="gpu-row">
                <span class="chip" class:ok=trigger_armed class:off=move || !trigger_armed()>
                    <span class="dot"></span>
                    {trigger_label}
                </span>
            </div>
            <crate::pages::benchmarks::GpuBenchRow open=open/>
            <div class="gpu-k">"Running local models"</div>
            <div class="pop-list gpu-list">
                {move || {
                    let rows = rows();
                    if rows.is_empty() {
                        return view! {
                            <div class="dim">"None — each model starts with its first request."</div>
                        }
                            .into_any();
                    }
                    rows.into_iter()
                        .map(|(r, bytes)| {
                            let state = if r.state == "ready" {
                                format!("up {}", age(r.started_at_age_seconds))
                            } else {
                                r.state.clone()
                            };
                            let busy_now = (r.in_flight > 0).then(|| format!("{} in flight", r.in_flight));
                            // Ladder design §6: which rung this resident runs,
                            // and — while a climb is in progress — its target
                            // and why. Both `None` for a row without a
                            // ladder, so this changes nothing for one.
                            let rung = r.rung.as_ref().map(|rung| format!("rung {}/{}", rung.rung, rung.of));
                            let climbing = r
                                .climbing
                                .as_ref()
                                .map(|c| format!("climbing to {}/{} — {}", c.to, c.of, c.reason));
                            // Ownership (candidate-aliases design §4.4, §6):
                            // absent is the owner's, folded into the same
                            // meta line as everything else about this
                            // resident.
                            let owner = (r.owner.as_deref() == Some("background"))
                                .then(|| "background".to_string());
                            let draining =
                                r.draining_for_owner.then(|| "draining for owner".to_string());
                            view! {
                                <div class="gpu-row">
                                    <span class="mono-sm gpu-model" title=r.container_name.clone()>
                                        {r.model_id.clone()}
                                    </span>
                                    <span class="type-badge">{r.class.clone()}</span>
                                    <span class="dim gpu-meta">
                                        {[
                                            Some(state),
                                            busy_now,
                                            bytes.map(|b| format!("~{}", human_bytes(b))),
                                            rung,
                                            climbing,
                                            owner,
                                            draining,
                                        ]
                                            .into_iter()
                                            .flatten()
                                            .collect::<Vec<_>>()
                                            .join(" · ")}
                                    </span>
                                </div>
                            }
                        })
                        .collect_view()
                        .into_any()
                }}
            </div>
            <div class="gpu-k">"Hold"</div>
            <div class="gpu-row">
                <span class="chip" class:warn=held class:off=move || !held()>
                    <span class="dot"></span>
                    {move || if held() { "engaged — local models are paused" } else { "off — local models start on demand" }}
                </span>
            </div>
            {move || {
                let d = draining();
                (!d.is_empty())
                    .then(|| {
                        view! {
                            <div class="gpu-row dim">
                                {format!("Draining: {}", d.join(", "))}
                            </div>
                        }
                    })
            }}
            <div class="gpu-row dim">
                {move || match fallback() {
                    Some(a) => view! {
                        "Held chat requests go to "
                        <code class="mono-sm">{a}</code>
                    }
                    .into_any(),
                    None => view! { "No fallback: held chat requests are refused" }.into_any(),
                }}
                " · "
                <span on:click=close>
                    <A href=crate::pages::settings_href("hold.fallback_alias")>
                        "Settings → GPU → Hold"
                    </A>
                </span>
            </div>
            <div class="gpu-row gpu-act">
                <button
                    type="button"
                    class="btn sm"
                    disabled=move || busy.get()
                    title=move || {
                        if held() {
                            "Let local models start again"
                        } else {
                            "Stop every local model and refuse or reroute local requests until released"
                        }
                    }
                    on:click=move |_| crate::model_ops::hold_set(toasts, busy, !held())
                >
                    {move || match (busy.get(), held()) {
                        (true, _) => "Working…",
                        (false, true) => "Release hold",
                        (false, false) => "Engage hold",
                    }}
                </button>
                <span class="applies-now">"applies now"</span>
            </div>
        </div>
    }
}

/// One sidebar entry. `icon` is the inner path/shape markup of a 16×16
/// stroke-based SVG (see app.css `.nav-item svg`). `<A>` does not prepend the
/// router base to hrefs, so it is done here — otherwise links escape the SPA
/// into the old UI's routes.
#[component]
fn NavItem(
    href: &'static str,
    label: &'static str,
    icon: AnyView,
    /// Exact-match active state; needed for the root link, which would
    /// otherwise be marked active on every page.
    #[prop(default = false)]
    exact: bool,
    /// Count to show on the right of the entry; rendered only when non-zero.
    /// A queue nobody sees is a queue nobody works: Docs' pending agent
    /// doc-requests, running downloads, MCP servers in error.
    #[prop(optional, into)]
    badge: Option<Signal<u64>>,
    /// What the count is, for its tooltip (the only place a rail shows it).
    #[prop(optional, into)]
    badge_title: Option<Signal<String>>,
    /// Red, for something broken; amber (waiting on you) otherwise.
    #[prop(optional)]
    bad: bool,
    /// A second count of another kind, after the first: Backends' updates
    /// available next to its live runs. Blue and marked "↑", so the two
    /// numbers are not read as one; rendered only when non-zero.
    #[prop(optional, into)]
    news: Option<Signal<u64>>,
    #[prop(optional, into)] news_title: Option<Signal<String>>,
) -> impl IntoView {
    // The title is the only label a rail has.
    view! {
        <A href=href exact=exact attr:class="nav-item" attr:title=label>
            <svg viewBox="0 0 16 16">{icon}</svg>
            <span class="nav-label">{label}</span>
            {badge
                .map(|b| {
                    view! {
                        <Show when=move || { b.get() > 0 }>
                            <span
                                class="nav-badge"
                                class:bad=bad
                                title=move || badge_title.map(|t| t.get()).unwrap_or_default()
                            >
                                {move || b.get()}
                            </span>
                        </Show>
                    }
                })}
            {news
                .map(|n| {
                    view! {
                        <Show when=move || { n.get() > 0 }>
                            <span
                                class="nav-badge news"
                                title=move || news_title.map(|t| t.get()).unwrap_or_default()
                            >
                                {move || format!("\u{2191}{}", n.get())}
                            </span>
                        </Show>
                    }
                })}
        </A>
    }
}

#[component]
pub fn Sidebar() -> impl IntoView {
    // The sidebar lives as long as the app, inside the router: the place to
    // keep the query every view was last shown with, for its tabs.
    crate::url_state::remember_queries();
    let pending = crate::pages::docs::use_docs_pending();
    let docs_badge = Signal::derive(move || pending.0.get());
    let docs_title = Signal::derive(move || format!("{} pending doc requests", pending.0.get()));
    let live = crate::live::use_live();
    let downloads = Signal::derive(move || {
        live.jobs.with(|j| {
            j.iter()
                .flatten()
                .filter(|j| j.kind == "hf_download")
                .count() as u64
        })
    });
    let downloads_title = Signal::derive(move || match downloads.get() {
        1 => "1 download running".to_string(),
        n => format!("{n} downloads running"),
    });
    // Live build runs, the same way: a run is a `build_run` job on the feed.
    // (The builds-with-updates count joins it with the update check.)
    let builds = Signal::derive(move || {
        live.jobs.with(|j| {
            j.iter()
                .flatten()
                .filter(|j| j.kind == crate::pages::backends::RUN_KIND)
                .count() as u64
        })
    });
    let builds_title = Signal::derive(move || match builds.get() {
        1 => "1 build running".to_string(),
        n => format!("{n} builds running"),
    });
    // A benchmark run: one at a time, it holds the whole GPU.
    let (bench_badge, bench_title) = crate::pages::benchmarks::nav_badge(live);
    // Updates available (container builds §8): builds with newer inputs than
    // their last verified run, plus registry images in use with a newer
    // digest. Pushed on the bus; nothing polls for it.
    let updates = Signal::derive(move || {
        live.updates.with(|u| {
            u.as_ref().map_or(0, |u| {
                u64::from(u.builds_with_updates) + u64::from(u.images_with_updates)
            })
        })
    });
    let updates_title = Signal::derive(move || {
        live.updates
            .with(|u| u.as_ref().map(crate::pages::backends::updates_title))
            .unwrap_or_default()
    });
    let mcp_errors = Memo::new(move |_| {
        live.mcp.with(|m| {
            m.iter()
                .flatten()
                .filter(|s| s.status == "error")
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        })
    });
    let mcp_badge = Signal::derive(move || mcp_errors.with(Vec::len) as u64);
    let mcp_title = Signal::derive(move || {
        mcp_errors.with(|names| match names.len() {
            1 => format!("1 MCP server in error: {}", names[0]),
            n => format!("{n} MCP servers in error: {}", names.join(", ")),
        })
    });
    let state = expect_context::<SidebarState>();
    let toggle_title = move || {
        if state.narrow.get() {
            "window too narrow to expand"
        } else if state.rail.get() {
            "Expand the sidebar"
        } else {
            "Collapse the sidebar to icons"
        }
    };
    let gpu_open = RwSignal::new(false);
    let gpu_anchor: NodeRef<html::Button> = NodeRef::new();
    // Overview first and alone: it is where a session starts, and the groups
    // under it are the places it links out to. The gateway's state sits at
    // the foot, where a desktop app keeps its status bar, in what is
    // otherwise the empty end of the column.
    view! {
        <nav class="sidebar">
            // The groups scroll on a short window; the foot below them does
            // not, so the gateway's state is always on screen.
            <div class="nav-scroll">
                <div class="nav-group">
                    <NavItem
                        href="/"
                        label="Overview"
                        exact=true
                        icon=view! { <path d="M2 13.5V9m4 4.5V6m4 7.5V3m4 10.5V7"/> }.into_any()
                    />
                </div>
                <div class="nav-group">
                    <div class="nav-eyebrow">"Use"</div>
                    <NavItem
                        href="/chat"
                        label="Chat"
                        icon=view! { <path d="M2.5 3.5h11v7h-6l-3 3v-3h-2z"/> }.into_any()
                    />
                    <NavItem
                        href="/audio-lab"
                        label="Audio lab"
                        icon=view! { <path d="M2 8h2M5 4v8M8 6v4M11 3v10M14 7v2"/> }.into_any()
                    />
                    <NavItem
                        href="/image-lab"
                        label="Image lab"
                        icon=view! {
                            <rect x="2.5" y="3" width="11" height="10" rx="1.5"/>
                            <circle cx="6" cy="6.5" r="1.1"/>
                            <path d="M3 11.5l3-3 2.5 2.5 2-1.5 3 2.5"/>
                        }
                            .into_any()
                    />
                    <NavItem
                        href="/agents"
                        label="Agents"
                        icon=view! {
                            <rect x="3" y="4.5" width="10" height="8" rx="2.5"/>
                            <circle cx="6.2" cy="8.2" r=".9"/>
                            <circle cx="9.8" cy="8.2" r=".9"/>
                            <path d="M8 2.2v2.3"/>
                        }
                            .into_any()
                    />
                </div>
                <div class="nav-group">
                    <div class="nav-eyebrow">"Serve"</div>
                    <NavItem
                        href="/models"
                        label="Models"
                        icon=view! {
                            <rect x="2.5" y="2.5" width="11" height="4.5" rx="1.5"/>
                            <rect x="2.5" y="9" width="11" height="4.5" rx="1.5"/>
                        }
                            .into_any()
                    />
                    <NavItem
                        href="/downloads"
                        label="Downloads"
                        badge=downloads
                        badge_title=downloads_title
                        icon=view! { <path d="M8 2v5m0 0l-3.5 3.5M8 7l3.5 3.5M2.5 13.5h11"/> }
                            .into_any()
                    />
                    <NavItem
                        href="/backends"
                        label="Backends"
                        badge=builds
                        badge_title=builds_title
                        news=updates
                        news_title=updates_title
                        icon=view! {
                            <path d="M8 2l5.5 2.75L8 7.5 2.5 4.75z"/>
                            <path d="M2.5 8L8 10.75 13.5 8"/>
                            <path d="M2.5 11.25L8 14l5.5-2.75"/>
                        }
                            .into_any()
                    />
                    <NavItem
                        href="/benchmarks"
                        label="Benchmarks"
                        badge=bench_badge
                        badge_title=bench_title
                        icon=view! {
                            <path d="M2.5 11.5a5.5 5.5 0 0 1 11 0"/>
                            <path d="M8 11.5l2.6-3.4"/>
                            <path d="M2 13.5h12"/>
                        }
                            .into_any()
                    />
                    <NavItem
                        href="/docs"
                        label="Docs"
                        badge=docs_badge
                        badge_title=docs_title
                        icon=view! {
                            <path d="M3 2.5h5.5a2 2 0 012 2v9a1.6 1.6 0 00-1.6-1.6H3z"/>
                            <path d="M13 2.5H9.9a1.6 1.6 0 00-1.4.9"/>
                            <path d="M13 2.5v11H9.9a1.6 1.6 0 00-1.4.9"/>
                        }
                            .into_any()
                    />
                    <NavItem
                        href="/knowledge"
                        label="Knowledge"
                        icon=view! {
                            <ellipse cx="8" cy="4" rx="5" ry="1.9"/>
                            <path d="M3 4v4c0 1 2.2 1.9 5 1.9s5-.9 5-1.9V4"/>
                            <path d="M3 8v4c0 1 2.2 1.9 5 1.9s5-.9 5-1.9V8"/>
                        }
                            .into_any()
                    />
                </div>
                <div class="nav-group">
                    <div class="nav-eyebrow">"Connect"</div>
                    <NavItem
                        href="/upstreams"
                        label="Upstreams"
                        icon=view! { <path d="M5 2.5v4a3 3 0 006 0v-4M8 9.5v4"/> }.into_any()
                    />
                    <NavItem
                        href="/mcp-servers"
                        label="MCP servers"
                        badge=mcp_badge
                        badge_title=mcp_title
                        bad=true
                        icon=view! {
                            <circle cx="4" cy="8" r="1.7"/>
                            <circle cx="12" cy="4" r="1.7"/>
                            <circle cx="12" cy="12" r="1.7"/>
                            <path d="M5.6 7.2l4.8-2.4M5.6 8.8l4.8 2.4"/>
                        }
                            .into_any()
                    />
                    <NavItem
                        href="/api-reference"
                        label="API reference"
                        icon=view! {
                            <path d="M5.5 3C4 3 4 4 4 5v1.5C4 7.5 3 8 2.5 8 3 8 4 8.5 4 9.5V11c0 1 0 2 1.5 2M10.5 3C12 3 12 4 12 5v1.5c0 1 1 1.5 1.5 1.5-.5 0-1.5.5-1.5 1.5V11c0 1 0 2-1.5 2"/>
                        }
                            .into_any()
                    />
                </div>
                <div class="nav-group">
                    <div class="nav-eyebrow">"Observe"</div>
                    <NavItem
                        href="/traffic"
                        label="Traffic"
                        icon=view! { <path d="M2 8h3l2-4 2 8 2-4h3"/> }.into_any()
                    />
                    <NavItem
                        href="/usage"
                        label="Usage"
                        icon=view! {
                            <path d="M2.5 13.5v-4M6.5 13.5V6M10.5 13.5V8.5M14.5 13.5v-9"/>
                            <path d="M1.5 2.5h1.5"/>
                        }
                            .into_any()
                    />
                    <NavItem
                        href="/wiring"
                        label="Wiring"
                        icon=view! {
                            <circle cx="3.5" cy="8" r="1.5"/>
                            <circle cx="12.5" cy="3.5" r="1.5"/>
                            <circle cx="12.5" cy="12.5" r="1.5"/>
                            <path d="M5 8h3m0 0V4.5c0-1 .5-1 1.5-1H11m-3 4.5v3.5c0 1 .5 1 1.5 1H11"/>
                        }
                            .into_any()
                    />
                </div>
                <div class="nav-group">
                    <NavItem
                        href="/settings"
                        label="Settings"
                        icon=view! {
                            <circle cx="8" cy="8" r="2.2"/>
                            <path d="M8 2v2M8 12v2M2 8h2M12 8h2M3.8 3.8l1.4 1.4M10.8 10.8l1.4 1.4M12.2 3.8l-1.4 1.4M5.2 10.8l-1.4 1.4"/>
                        }
                            .into_any()
                    />
                </div>
            </div>
            <div class="foot">
                <Pulse/>
                <div class="foot-row">
                    <GpuPill open=gpu_open anchor=gpu_anchor/>
                    <button
                        type="button"
                        class="nav-toggle"
                        title=toggle_title
                        aria-label=toggle_title
                        disabled=move || state.narrow.get()
                        on:click=move |_| state.toggle()
                    >
                        {move || if state.rail.get() { "»" } else { "«" }}
                    </button>
                </div>
            </div>
        </nav>
        <Popover open=gpu_open anchor=gpu_anchor class="gpu-pop" min_width=340>
            <GpuPanel open=gpu_open/>
        </Popover>
    }
}
