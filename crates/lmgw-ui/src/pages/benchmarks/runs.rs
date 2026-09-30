//! The runs table (benchmark design §8.2): one row per run, newest first —
//! the running one on top with its stage and progress off the jobs feed —
//! with its headline numbers, its probes as passed of judged, its verdict
//! against the previous comparable run, and its status. Rows are picked for
//! Compare in the order they are ticked; the first one is the base.

use leptos::prelude::*;
use leptos_router::components::A;
use lmgw_api_types::bench_ops::{BenchArgs, BenchRunSummary, BenchStatus};

use super::delta::ThresholdControl;
use super::{
    build_words, compare_href, fmt_ms, fmt_rate, fmt_tokj, run_href, status_look, use_bn, Bn,
};
use crate::bench_api::LiveBench;
use crate::fmt::{hue_for, human_bytes};
use crate::pages::backends::{ago, local_ts, sha7};
use crate::widgets::{ConfirmButton, MenuItem, PageFrame, PageMode, RowMenu};

/// The table's column count.
const COLS: u32 = 14;

#[component]
pub fn RunsView() -> impl IntoView {
    let bn = use_bn();
    let picked = RwSignal::new(Vec::<i64>::new());
    let ids = Memo::new(move |_| {
        bn.runs.with(|r| {
            r.as_ref()
                .map(|r| r.runs.iter().map(|s| s.id).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    // A pick whose run went away is dropped.
    Effect::new(move |_| {
        let ids = ids.get();
        if picked.with_untracked(|p| p.iter().any(|id| !ids.contains(id))) {
            picked.update(|p| p.retain(|id| ids.contains(id)));
        }
    });
    let empty = move || {
        bn.runs
            .with(|r| r.as_ref().is_some_and(|r| r.runs.is_empty()))
    };
    let count = move || {
        bn.runs
            .with(|r| r.as_ref().map(|r| r.runs.len()))
            .map(|n| crate::fmt::count_of(n, "runs"))
            .unwrap_or_default()
    };
    let sub = move || {
        format!(
            "{} · speed, energy and behaviour of a local chat row on a llama-server image",
            count()
        )
    };
    view! {
        <PageFrame
            title="Benchmarks"
            sub=Signal::derive(sub)
            mode=PageMode::Fill
            class="bench"
            actions=move || {
                view! {
                    <button
                        class="btn primary"
                        title="Pick a row, an image and the phases, see the plan, then start"
                        on:click=move |_| bn.open_new(None)
                    >
                        "New benchmark"
                    </button>
                }
            }
            toolbar=move || view! { <Toolbar picked=picked/> }
        >
            {move || {
                bn.runs_err
                    .get()
                    .map(|e| {
                        view! {
                            <div class="notice err row">
                                "Loading the runs failed: "
                                {e}
                                <button class="btn ghost sm" on:click=move |_| bn.load_runs()>
                                    "Retry"
                                </button>
                            </div>
                        }
                    })
            }}
            <div class="fill-pane card pad0">
                <Show when=empty fallback=move || view! { <RunsTable ids=ids picked=picked/> }>
                    <EmptyState/>
                </Show>
            </div>
        </PageFrame>
    }
}

#[component]
fn Toolbar(picked: RwSignal<Vec<i64>>) -> impl IntoView {
    let bn = use_bn();
    let n = move || picked.with(Vec::len);
    view! {
        <div class="bn-bar">
            <button
                class="btn sm"
                disabled=move || n() < 2
                title="Overlay the picked runs; the first one picked is the base the others are judged against"
                on:click=move |_| bn.go(compare_href(&picked.get_untracked()))
            >
                {move || match n() {
                    0 | 1 => "Compare picked runs".to_string(),
                    k => format!("Compare {k} runs"),
                }}
            </button>
            <Show when=move || { n() > 0 }>
                <span class="dim bn-pick-note">
                    {move || {
                        picked
                            .with(|p| p.first().copied())
                            .map(|b| format!("base: run {b}"))
                            .unwrap_or_default()
                    }}
                </span>
                <button class="btn ghost sm" on:click=move |_| picked.set(Vec::new())>
                    "Clear"
                </button>
            </Show>
            <span class="spacer"></span>
            <ThresholdControl/>
        </div>
    }
}

#[component]
fn EmptyState() -> impl IntoView {
    let bn = use_bn();
    view! {
        <div class="bk-empty">
            <p>
                <b>"No benchmark runs yet."</b>
                " A run measures one local chat row on one llama-server image: load time and VRAM, "
                "whether the knobs lmgw relies on work (thinking on and off, tool calls, JSON schema, "
                "vision), prefill and decode speed across the row's whole context, concurrent and "
                "mixed load, and tokens per joule. It needs the whole GPU while it runs."
            </p>
            <div class="row">
                <button class="btn primary" on:click=move |_| bn.open_new(None)>
                    "New benchmark"
                </button>
            </div>
        </div>
    }
}

#[component]
fn RunsTable(ids: Memo<Vec<i64>>, picked: RwSignal<Vec<i64>>) -> impl IntoView {
    view! {
        <table class="data bn-runs">
            <thead>
                <tr>
                    <th class="pick" title="Pick runs to compare"></th>
                    <th>"Run"</th>
                    <th>"Build"</th>
                    <th class="num-h" title="Prefill tokens per second at 2048 tokens, or the nearest measured length">
                        "Prefill"
                    </th>
                    <th class="num-h bn-a" title="Time to first token at that same length">"TTFT"</th>
                    <th class="num-h" title="Decode tokens per second at depth 64">"Decode"</th>
                    <th class="num-h bn-a" title="Aggregate decode tokens per second with every slot streaming">
                        "Aggregate"
                    </th>
                    <th class="num-h bn-b" title="Decode tokens per joule at depth 64 (the whole card)">"tok/J"</th>
                    <th class="num-h bn-c" title="podman run → /health answering">"Load"</th>
                    <th class="num-h bn-c" title="VRAM the load took (after load minus the baseline)">"VRAM"</th>
                    <th class="num-h bn-d" title="Probes that passed, of those that gave a verdict">"Probes"</th>
                    <th class="bn-d" title="Against the previous comparable run: same weights, settings, suite and GPU">
                        "vs previous"
                    </th>
                    <th>"Status"</th>
                    <th class="actions"></th>
                </tr>
            </thead>
            <tbody>
                <For each=move || ids.get() key=|id| *id let:id>
                    <RunRow id=id picked=picked/>
                </For>
            </tbody>
        </table>
    }
}

fn dash() -> String {
    "—".to_string()
}

/// The seed "Benchmark again" opens the modal with: the same row, rung,
/// image, phases and repetitions.
pub fn again_seed(s: &BenchRunSummary) -> BenchArgs {
    BenchArgs {
        model_id: s.model_id.clone(),
        rung: s.rung,
        image: Some(s.image_ref.clone()).filter(|i| !i.is_empty()),
        phases: s.phases.clone(),
        repetitions: Some(s.repetitions),
        ..Default::default()
    }
}

#[component]
fn RunRow(id: i64, picked: RwSignal<Vec<i64>>) -> impl IntoView {
    let bn = use_bn();
    // A row whose run just left the list keeps its last view until the
    // `For` drops it.
    let s = Memo::new(move |prev: Option<&BenchRunSummary>| {
        bn.runs
            .with(|r| {
                r.as_ref()
                    .and_then(|r| r.runs.iter().find(|s| s.id == id).cloned())
            })
            .or_else(|| prev.cloned())
            .unwrap_or_default()
    });
    let live = Memo::new(move |_| bn.live.with(|l| l.clone().filter(|l| l.run_id == id)));
    let running = Memo::new(move |_| {
        live.with(Option::is_some) || s.with(|s| s.status == BenchStatus::Running)
    });
    let threshold = Memo::new(move |_| {
        bn.runs
            .with(|r| r.as_ref().map(|r| r.threshold_pct))
            .unwrap_or_else(|| bn.threshold.get())
    });
    let is_picked = move || picked.with(|p| p.contains(&id));
    let pick = move |_| {
        picked.update(|p| {
            if let Some(i) = p.iter().position(|x| *x == id) {
                p.remove(i);
            } else {
                p.push(id);
            }
        })
    };

    let run_cell = move || {
        let (model, rung, notes, quant, created) = s.with(|s| {
            (
                s.model_id.clone(),
                s.rung,
                s.notes.clone(),
                s.quant.clone(),
                s.created_at.clone(),
            )
        });
        let hue = hue_for(&model);
        let rung = (rung > 0).then(|| {
            view! { <span class="type-badge bn-rung">{format!("rung {rung}")}</span> }
        });
        let quant_badge = quant
            .clone()
            .map(|q| view! { <span class="type-badge bn-quant" title="quant, from the GGUF header">{q}</span> });
        let mut title = format!("Run {id} of {model}");
        if let Some(q) = &quant {
            title.push_str(&format!(" · {q}"));
        }
        if !notes.is_empty() {
            title.push_str(&format!("\n{notes}"));
        }
        view! {
            <div class="bn-run-cell">
                <A href=run_href(id) attr:class="row-link bn-run-link" attr:title=title>
                    <span class="pfx mono-sm bn-id">{format!("#{id}")}</span>
                    <i class="swatch" style=format!("--hue:{hue}")></i>
                    <span class="mono-sm">{model}</span>
                </A>
                {quant_badge}
                {rung}
                <span class="dim bn-age" title=local_ts(&created)>{ago(&created)}</span>
            </div>
        }
    };
    let build = move || {
        s.with(|s| {
            build_words(
                &s.image_ref,
                s.engine_slug.as_deref(),
                s.version.as_deref(),
                s.commit.as_deref(),
            )
        })
    };
    let build_title = move || {
        s.with(|s| {
            let mut t = s.image_ref.clone();
            if let Some(c) = &s.commit {
                t.push_str(&format!("\ncommit {}", sha7(c)));
            }
            if let Some(b) = &s.build_info {
                t.push_str(&format!("\nbuild_info {b}"));
            }
            t
        })
    };
    let h = move |f: fn(&BenchRunSummary) -> Option<String>| move || s.with(f).unwrap_or_else(dash);
    let prefill = h(|s| s.headline.prefill_tok_s.map(fmt_rate));
    let prefill_title = move || {
        s.with(|s| {
            s.headline
                .prefill_tokens
                .map(|t| format!("prefill tok/s at {t} tokens"))
                .unwrap_or_else(|| "not measured".into())
        })
    };
    let ttft = h(|s| s.headline.ttft_ms.map(fmt_ms));
    let decode = h(|s| s.headline.decode_tok_s.map(fmt_rate));
    let decode_title = move || {
        s.with(
            |s| match (s.headline.decode_deep_tok_s, s.headline.decode_deep_depth) {
                (Some(v), Some(d)) => format!(
                    "decode tok/s at depth 64; {} at the deepest point, {d}",
                    fmt_rate(v)
                ),
                _ => "decode tok/s at depth 64".into(),
            },
        )
    };
    let aggregate = h(|s| s.headline.aggregate_tok_s.map(fmt_rate));
    let aggregate_title = move || {
        s.with(|s| {
            s.headline
                .aggregate_streams
                .map(|n| format!("aggregate tok/s with {n} streams"))
                .unwrap_or_else(|| "not measured".into())
        })
    };
    let tokj = h(|s| s.headline.decode_tokens_per_joule.map(fmt_tokj));
    let load = h(|s| s.headline.load_ms.map(|v| fmt_ms(v as f64)));
    let vram = h(|s| s.headline.load_vram_bytes.map(human_bytes));
    let probes = move || {
        s.with(|s| {
            if s.probes_judged == 0 {
                return view! { <span class="dim">"—"</span> }.into_any();
            }
            let all = s.probes_passed == s.probes_judged;
            let title = format!(
                "{} of {} probes with a verdict passed (info and skipped ones are not counted)",
                s.probes_passed, s.probes_judged
            );
            view! {
                <span class=if all { "bn-probes ok" } else { "bn-probes bad" } title=title>
                    {format!("{}/{}", s.probes_passed, s.probes_judged)}
                </span>
            }
            .into_any()
        })
    };
    let versus = move || {
        let t = threshold.get();
        s.with(|s| {
            let Some(prev) = s.previous_id else {
                return view! { <span class="dim" title="No earlier comparable run: same weights, settings, suite and GPU">"—"</span> }
                    .into_any();
            };
            let (r, i) = (s.regressions.unwrap_or(0), s.improvements.unwrap_or(0));
            let title = format!(
                "Against run {prev} at a {t} % threshold (widened by the measured noise): {r} worse, {i} better. Click to compare."
            );
            let href = compare_href(&[prev, s.id]);
            view! {
                <A href=href attr:class="row-link bn-vs" attr:title=title>
                    {(r > 0).then(|| view! { <span class="chip err bn-vs-chip">{format!("▼ {r}")}</span> })}
                    {(i > 0).then(|| view! { <span class="chip ok bn-vs-chip">{format!("▲ {i}")}</span> })}
                    {(r == 0 && i == 0).then(|| view! { <span class="dim">"same"</span> })}
                </A>
            }
            .into_any()
        })
    };
    let status = move || {
        if let Some(l) = live.get() {
            let words = match l.percent {
                Some(p) => format!("{} {p}%", l.phase_word()),
                None => l.phase_word(),
            };
            return view! {
                <span class="chip live" title=l.stage.clone()>
                    <span class="dot"></span>
                    {words}
                </span>
            }
            .into_any();
        }
        s.with(|s| {
            let (class, words, title) = status_look(
                s.status,
                s.complete,
                s.throttled,
                s.status_reason.as_deref(),
                s.error.as_deref(),
            );
            let title = format!(
                "{title}\nstarted {}{}",
                local_ts(&s.created_at),
                s.finished_at
                    .as_deref()
                    .map(|f| format!(", ended {}", local_ts(f)))
                    .unwrap_or_default()
            );
            // Stored columns that did not decode (decision 51): listed with
            // what could be read, compared with nothing.
            let unreadable = (!s.unreadable.is_empty()).then(|| {
                let tip = format!(
                    "Part of this run's stored data could not be read, so it is compared with nothing:\n{}",
                    s.unreadable.join("\n")
                );
                view! { <span class="chip warn bn-unreadable" title=tip>"unreadable"</span> }
            });
            view! {
                <span class=class title=title>
                    <span class="dot"></span>
                    {words}
                </span>
                {unreadable}
            }
            .into_any()
        })
    };
    let act = move || {
        if running.get() {
            view! {
                <ConfirmButton
                    label="Cancel"
                    confirm=format!("Cancel run {id}?")
                    title="Stop the run: its container is removed, the GPU released, the phases measured so far kept"
                    on_confirm=Callback::new(move |()| bn.cancel(id))
                />
            }
            .into_any()
        } else {
            view! {
                <ConfirmButton
                    label="Delete"
                    confirm=format!("Delete run {id}?")
                    title="Delete the run and everything it measured"
                    on_confirm=Callback::new(move |()| bn.delete(id, None))
                />
            }
            .into_any()
        }
    };
    let menu = Signal::derive(move || row_menu(bn, id, s, running.get()));
    view! {
        <tr class:bn-live=move || running.get() class:bn-picked=is_picked>
            <td class="pick">
                <input
                    type="checkbox"
                    title="Pick for Compare"
                    aria-label=format!("Pick run {id} for Compare")
                    prop:checked=is_picked
                    on:change=pick
                />
            </td>
            <td class="clip">{run_cell}</td>
            <td class="mono-sm bn-cut" title=build_title>{build}</td>
            <td class="num" title=prefill_title>{prefill}</td>
            <td class="num bn-a">{ttft}</td>
            <td class="num" title=decode_title>{decode}</td>
            <td class="num bn-a" title=aggregate_title>{aggregate}</td>
            <td class="num bn-b">{tokj}</td>
            <td class="num bn-c">{load}</td>
            <td class="num bn-c">{vram}</td>
            <td class="num bn-d">{probes}</td>
            <td class="bn-d">{versus}</td>
            <td>{status}</td>
            <td class="actions">
                <span class="row-acts">{act} <RowMenu items=menu/></span>
            </td>
        </tr>
        {move || live.get().map(|l| view! { <LiveLine l=l/> })}
    }
}

fn row_menu(bn: Bn, id: i64, s: Memo<BenchRunSummary>, running: bool) -> Vec<MenuItem> {
    let prev = s.with(|s| s.previous_id);
    let mut items = vec![
        MenuItem::new("Open", move || bn.go(run_href(id))),
        MenuItem::new("Compare with previous", move || {
            if let Some(p) = prev {
                bn.go(compare_href(&[p, id]));
            }
        })
        .disabled(prev.is_none())
        .title(match prev {
            Some(p) => format!("Overlay run {p}, the previous comparable run, and this one"),
            None => "No earlier comparable run: same weights, settings, suite and GPU".into(),
        }),
        MenuItem::new("Benchmark again…", move || {
            bn.open_new(Some(s.with_untracked(again_seed)))
        })
        .title("The New benchmark modal with this run's row, rung, image, phases and repetitions"),
        MenuItem::new("Notes…", move || {
            bn.notes
                .set(Some((id, s.with_untracked(|s| s.notes.clone()))))
        }),
    ];
    if running {
        items.push(
            MenuItem::new("Cancel run", move || bn.cancel(id))
                .danger()
                .title("Stop the run; the phases measured so far are kept"),
        );
    }
    items
}

/// The running run's stage and progress, across the row.
#[component]
fn LiveLine(l: LiveBench) -> impl IntoView {
    let pct = l.percent;
    let steps = super::steps_words(l.done, l.total);
    view! {
        <tr class="detail-row bn-live-row">
            <td colspan=COLS>
                <div class="bn-live-line">
                    <span class="bn-stage" title=l.stage.clone()>{l.stage.clone()}</span>
                    <span class="dim">{steps}</span>
                    <div class="progress bn-progress" title=pct.map(|p| format!("{p}%"))>
                        <i style=format!("width:{}%", pct.unwrap_or(0))></i>
                    </div>
                </div>
            </td>
        </tr>
    }
}
