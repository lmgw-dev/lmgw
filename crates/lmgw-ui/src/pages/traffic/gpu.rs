//! The GPU plane on Traffic (quickdoc §9b): the ledger this page reads, the
//! header's button and the panel it opens, and the starts waiting for a
//! download.

use leptos::html;
use leptos::prelude::*;
use lmgw_api_types::VramStatus;

use crate::fmt::{fmt_bytes, hue_for};
use crate::live::use_live;
use crate::widgets::{Ico, Icon, Popover};

/// The GPU ledger for this page: the bus when it has spoken, else one fetch
/// — a page opened mid-session should not wait for the next tick.
pub(super) fn gpu_state() -> Memo<Option<VramStatus>> {
    let live = use_live();
    let initial = LocalResource::new(|| crate::api::get::<VramStatus>("/api/vram"));
    Memo::new(move |_| {
        live.vram
            .get()
            .or_else(|| initial.get().and_then(|r| r.ok()))
            .filter(|v| v.enabled)
    })
}

/// The GPU on the list's header bar: a chip icon whose tooltip says the free
/// memory, with a count while anything waits for the card; a click opens the
/// panel in a popover over the list. With nothing selected the panel is on
/// the status page too.
#[component]
pub(super) fn GpuButton(vram: Memo<Option<VramStatus>>) -> impl IntoView {
    let open = RwSignal::new(false);
    let btn: NodeRef<html::Button> = NodeRef::new();
    let waiting = Memo::new(move |_| vram.with(|v| v.as_ref().map_or(0, |v| v.queue.len())));
    let fetching = Memo::new(move |_| {
        vram.with(|v| {
            v.as_ref()
                .map_or(0, |v| crate::gpu_download::waiting(v).len())
        })
    });
    let title = move || {
        vram.with(|v| {
            let Some(v) = v else {
                return "GPU memory".to_string();
            };
            let mut t = match v.free_bytes {
                Some(f) => format!(
                    "GPU · {} free of {}",
                    fmt_bytes(f),
                    fmt_bytes(v.capacity_bytes)
                ),
                None => "GPU".to_string(),
            };
            t.push_str(&format!(
                " · {} · {} resident",
                if v.active {
                    "admitting"
                } else {
                    "reporting only"
                },
                v.resident.len()
            ));
            match waiting.get() {
                0 => {}
                n => t.push_str(&format!(" · {n} waiting")),
            }
            match fetching.get() {
                0 => {}
                n => t.push_str(&format!(" · {n} downloading")),
            }
            t.push_str(" — click for what is resident and what waits");
            t
        })
    };
    view! {
        <Show when=move || vram.with(Option::is_some)>
            <button
                type="button"
                class="icon-btn gpu-btn"
                node_ref=btn
                aria-expanded=move || open.get().to_string()
                aria-label="GPU memory"
                title=title
                on:click=move |_| open.update(|o| *o = !*o)
            >
                <Icon ico=Ico::Chip/>
                <Show when=move || { waiting.get() + fetching.get() != 0 }>
                    <span class="gpu-badge">{move || waiting.get() + fetching.get()}</span>
                </Show>
            </button>
        </Show>
        <Popover open=open anchor=btn class="gpu-pop" min_width=360>
            <GpuPanel vram=vram/>
        </Popover>
    }
}

/// The GPU plane (quickdoc §9b): what is resident, what it is measured to cost,
/// and anything queued behind it.
///
/// It lives on Traffic because a request waiting for VRAM *is* traffic — it has
/// arrived, it is not being served yet, and the reason is here. Measured and
/// estimated figures are labelled apart on purpose: the estimate is weights +
/// KV + compute buffers derived from the GGUF and leaves the CUDA context out,
/// so presenting the two as one number would be the wrong kind of tidy.
#[component]
pub(super) fn GpuPanel(vram: Memo<Option<VramStatus>>) -> impl IntoView {
    // In use and kept free apart (`gpu_figures::bar`): what admission keeps
    // free for memory the driver cannot show yet is not in use, and counting
    // it as used made the bar disagree with the popover.
    let split = move || {
        vram.get()
            .as_ref()
            .and_then(crate::gpu_figures::bar)
            .unwrap_or((0.0, 0.0))
    };

    view! {
        <section class="card gpu-card">
        {move || {
            let v = vram.get().unwrap_or_default();
            let queue = v.queue.clone();
            let resident = v.resident.clone();
            let telemetry = v.telemetry.clone();
            let reason = v.inactive_reason.clone();
            let capacity = v.capacity_bytes;
            let free = v.free_bytes;
            let estimated = v.estimated_resident_bytes;
            let measured = v.free_measured;
            let hold_active = v.hold_active;
            let draining_n = v.draining.len();
            // The same lines as the popover's, so the two add up alike.
            let kept_line = crate::gpu_figures::kept_free_line(&v);
            // The kept part's words: what it is kept free for, a start
            // not measured yet among them.
            let kept_title = kept_line
                .clone()
                .unwrap_or_else(|| "kept free by admission".to_string());
            let holders = crate::gpu_figures::holders_line(Some(&v));
            view! {
                    <div class="row">
                        <strong>"GPU memory"</strong>
                        <span class=if v.active { "chip ok" } else { "chip off" }>
                            <span class="dot"></span>
                            {if v.active { "admitting" } else { "reporting only" }}
                        </span>
                        <span class="spacer" style="flex:1"></span>
                        <span class="dim mono-sm" title=telemetry.clone()>
                            {match free {
                                Some(f) => {
                                    format!(
                                        "{} free of {} ({})",
                                        fmt_bytes(f),
                                        fmt_bytes(capacity),
                                        if measured { "measured" } else { "derived from estimates" },
                                    )
                                }
                                None => telemetry.clone(),
                            }}
                        </span>
                    </div>
                    // `!= 0` rather than `> 0`: inside `view!` a bare `>`
                    // closes the tag.
                    <Show when=move || capacity != 0>
                        <div class="progress split" style="margin-top:8px">
                            <i
                                style=move || format!("width:{:.1}%", split().0)
                                title="in use"
                            ></i>
                            <i
                                class="kept"
                                style=move || format!("width:{:.1}%", split().1)
                                title=kept_title.clone()
                            ></i>
                        </div>
                    </Show>
                    {kept_line.map(|k| view! { <div class="dim mini-note">{k}</div> })}
                    <div class="dim mini-note">{holders}</div>
                    <div class="row" style="margin-top:8px">
                        {resident
                            .iter()
                            .map(|r| {
                                let hue = hue_for(&r.model);
                                // The learned peak rides beside the idle
                                // estimate rather than inside it: it is
                                // what admission keeps free on top, and
                                // the note in the tooltip says whether the
                                // row has one at all.
                                // What an audio model that has not loaded
                                // yet is about to take is part of its
                                // estimate, not on top of it: the driver
                                // does not show it yet, so admission keeps
                                // it free.
                                let label = format!(
                                    "{} · {} · {}{}{}",
                                    r.container,
                                    r.state,
                                    fmt_bytes(r.estimated_bytes),
                                    match r.peak_extra_bytes {
                                        Some(p) => format!(" + {} peak", fmt_bytes(p)),
                                        None => String::new(),
                                    },
                                    match r.pending_bytes {
                                        Some(p) => format!(", {} of it to load", fmt_bytes(p)),
                                        None => String::new(),
                                    },
                                );
                                let title = match (&r.note, r.idle_seconds) {
                                    (Some(n), _) => n.clone(),
                                    (None, Some(s)) => format!("idle {s}s"),
                                    (None, None) => "not routed to by lmgw yet".into(),
                                };
                                view! {
                                    <span class="model-chip" style=format!("--hue:{hue}") title=title>
                                        <i></i>
                                        {r.model.clone()}
                                        <span class="dim mono-sm">{label}</span>
                                        <Show when={
                                            let n = r.in_flight;
                                            move || n > 0
                                        }>
                                            <span class="chip info">
                                                <span class="dot"></span>
                                                "busy"
                                            </span>
                                        </Show>
                                    </span>
                                }
                            })
                            .collect_view()}
                        <Show when={
                            let empty = resident.is_empty();
                            move || empty
                        }>
                            <span class="dim mono-sm">"nothing resident"</span>
                        </Show>
                    </div>
                    <div class="dim mini-note">
                        {format!("estimated resident {}", fmt_bytes(estimated))}
                        {reason.map(|r| format!(" — {r}")).unwrap_or_default()}
                    </div>
                    <Show when=move || hold_active>
                        <div class="dim mini-note">{format!("Hold active · {draining_n} draining")}</div>
                    </Show>
                    <Show when={
                        let n = queue.len();
                        move || n > 0
                    }>
                        <div style="margin-top:10px">
                            <strong class="mono-sm">
                                {format!("{} waiting for GPU memory", queue.len())}
                            </strong>
                            {queue
                                .iter()
                                .map(|w| {
                                    view! {
                                        <div class="row mono-sm" style="margin-top:4px">
                                            <span class="type-badge">{w.position}</span>
                                            <span>{w.alias.clone()}</span>
                                            <span class="dim">
                                                {format!(
                                                    "needs {} · {} · waiting {:.1}s",
                                                    fmt_bytes(w.needs_bytes),
                                                    w.stage,
                                                    w.waiting_ms as f64 / 1000.0,
                                                )}
                                            </span>
                                        </div>
                                    }
                                })
                                .collect_view()}
                        </div>
                    </Show>
            }
        }}
        <GpuDownloads vram=vram/>
        </section>
    }
}

/// The starts that wait for their download before admission (machine-check
/// design §5): beside the queue, since they are waiting too, but holding no
/// place in it and nothing on the card. Outside the panel's redraw, so a
/// bar's motion is not restarted by every frame.
#[component]
fn GpuDownloads(vram: Memo<Option<VramStatus>>) -> impl IntoView {
    let waiting = Memo::new(move |_| {
        vram.with(|v| {
            v.as_ref()
                .map(crate::gpu_download::waiting)
                .unwrap_or_default()
        })
    });
    view! {
        <Show when=move || waiting.with(|w| !w.is_empty())>
            <div style="margin-top:10px">
                <strong class="mono-sm">
                    {move || match waiting.with(Vec::len) {
                        1 => "1 start waiting for its download".to_string(),
                        n => format!("{n} starts waiting for their download"),
                    }}
                </strong>
                <For each=move || waiting.get() key=|w| (w.0.clone(), w.1.clone()) let:w>
                    {
                        let (class, model, alias) = w;
                        let dl = crate::gpu_download::live_for(class.clone(), model.clone());
                        view! {
                            <div class="row mono-sm gpu-dl-wait" style="margin-top:4px">
                                <span class="type-badge">{class}</span>
                                <span title=format!("model {model}")>{alias}</span>
                            </div>
                            <crate::gpu_download::DlLine dl=Signal::derive(move || {
                                dl.get().unwrap_or_default()
                            })/>
                        }
                    }
                </For>
            </div>
        </Show>
    }
}
