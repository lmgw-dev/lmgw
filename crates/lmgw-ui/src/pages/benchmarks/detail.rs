//! One run whole (`?run=<id>`, benchmark design §8.2): what went wrong or is
//! worth knowing first (status, partial, throttled), the headline numbers
//! with their change against the previous comparable run, what the run was
//! (identity, settings, command line, notes), the probes, one chart per
//! phase, the timeline, and the comparison with its threshold.
//!
//! A running run fills in as it goes: `bench_run` is read again on each
//! progress tick of its job on the jobs feed — one read at a time, the tick
//! that lands during one queues exactly one more — and once more when the
//! job leaves the feed. No timer outlives the page.

use leptos::prelude::*;
use leptos_router::components::A;
use lmgw_api_types::bench::{ProbeOutcome, Timeline, CLOCK_EVENT_REASONS};
use lmgw_api_types::bench_ops::{
    BenchArgs, BenchComparison, BenchRun, BenchRunDetail, BenchStatus,
};
use lmgw_api_types::{LocalModel, ModelsFull};

use super::delta::{tally, DeltaTable, HeadlineTiles, ThresholdControl};
use super::identity::IdentityCards;
use super::notes::NotesEditor;
use super::phase_charts::{PhaseCharts, RunLine};
use super::probes::ProbesList;
use super::timeline::TimelineCard;
use super::{build_words, compare_href, fmt_ms, phase_label, tokens_short, use_bn, Bn};
use crate::bench_api as api;
use crate::pages::backends::{ago, local_ts};
use crate::scope::{Latest, Scope};
use crate::widgets::{ConfirmButton, PageFrame, Section};

/// The detail's reads: one in flight at a time, the newest answer applied.
#[derive(Clone, Copy)]
struct Loader {
    bn: Bn,
    id: i64,
    scope: Scope,
    data: RwSignal<Option<BenchRunDetail>>,
    err: RwSignal<Option<String>>,
    read: Latest,
    busy: StoredValue<bool>,
    again: StoredValue<bool>,
}

impl Loader {
    fn load(self) {
        if !self.scope.alive() {
            return;
        }
        if self.busy.get_value() {
            self.again.set_value(true);
            return;
        }
        let Some(ticket) = self.read.next() else {
            return;
        };
        self.busy.set_value(true);
        let threshold = self.bn.threshold.get_untracked();
        self.scope.spawn(async move {
            let res = api::run(self.id, threshold, true).await;
            self.busy.set_value(false);
            if self.read.is(ticket) {
                match res {
                    Ok(d) => {
                        self.err.set(None);
                        self.data.set(Some(d));
                    }
                    Err(e) => self.err.set(Some(e.to_string())),
                }
            }
            if self.again.get_value() {
                self.again.set_value(false);
                self.load();
            }
        });
    }
}

/// The run as the charts draw it: its points, without the timeline samples
/// (the timeline card has its own).
fn line_of(run: &BenchRun) -> RunLine {
    let mut r = run.clone();
    r.timeline = Timeline::default();
    RunLine {
        label: format!("run {}", run.id),
        color: "var(--c1)".into(),
        run: r,
    }
}

#[component]
pub fn DetailView(id: i64) -> impl IntoView {
    let bn = use_bn();
    let l = Loader {
        bn,
        id,
        scope: Scope::new(),
        data: RwSignal::new(None),
        err: RwSignal::new(None),
        read: Latest::new(),
        busy: StoredValue::new(false),
        again: StoredValue::new(false),
    };
    l.load();
    // Read again at a new threshold, after a change to a stored run, and on
    // every progress tick of this run's job — and once more when it ends.
    let tick = Memo::new(move |_| {
        bn.live.with(|v| {
            v.as_ref()
                .filter(|v| v.run_id == id)
                .map(|v| (v.done, v.stage.clone()))
        })
    });
    type Key = (u64, u64, Option<(u64, String)>);
    Effect::new(move |prev: Option<Key>| {
        let now = (bn.threshold.get().to_bits(), bn.rev.get(), tick.get());
        if prev.is_some_and(|p| p != now) {
            l.load();
        }
        now
    });

    let row = RwSignal::new(None::<LocalModel>);
    let model_id = Memo::new(move |_| {
        l.data
            .with(|d| d.as_ref().map(|d| d.run.model.model_id.clone()))
    });
    Effect::new(move |_| {
        let Some(mid) = model_id.get() else { return };
        l.scope.spawn(async move {
            if let Ok(m) = crate::api::get::<ModelsFull>("/api/models/full").await {
                row.set(
                    m.local
                        .into_iter()
                        .map(|v| v.model)
                        .find(|r| r.model_id == mid),
                );
            }
        });
    });

    let run = Memo::new(move |_| l.data.with(|d| d.as_ref().map(|d| d.run.clone())));
    let live = Memo::new(move |_| bn.live.with(|v| v.clone().filter(|v| v.run_id == id)));
    let running = Memo::new(move |_| {
        live.with(Option::is_some)
            || run.with(|r| r.as_ref().is_some_and(|r| r.status == BenchStatus::Running))
    });
    let sub = move || {
        run.with(|r| {
            r.as_ref()
                .map(|r| {
                    let mut parts = vec![r.model.model_id.clone()];
                    if r.model.rung > 0 {
                        parts.push(format!("rung {}", r.model.rung));
                    }
                    if let Some(q) = &r.model.quant {
                        parts.push(q.clone());
                    }
                    parts.push(build_words(
                        &r.image_ref,
                        r.build.engine_slug.as_deref(),
                        r.build.version.as_deref(),
                        r.build.commit.as_deref(),
                    ));
                    parts.push(local_ts(&r.created_at));
                    parts.join(" · ")
                })
                .unwrap_or_else(|| "loading…".into())
        })
    };
    let base = Memo::new(move |_| {
        l.data.with(|d| {
            d.as_ref()
                .and_then(|d| d.comparison.as_ref().map(|c| c.base_run_id))
        })
    });
    let actions = move || {
        let del = move || {
            if running.get() {
                view! {
                    <ConfirmButton
                        label="Cancel run"
                        confirm=format!("Cancel run {id}?")
                        class="btn"
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
                        class="btn"
                        title="Delete the run and everything it measured"
                        on_confirm=Callback::new(move |()| {
                            bn.delete(id, Some(Callback::new(move |()| bn.go("/benchmarks".into()))))
                        })
                    />
                }
                .into_any()
            }
        };
        view! {
            <A href="/benchmarks" attr:class="btn ghost">"All runs"</A>
            {move || {
                base.get()
                    .map(|b| {
                        view! {
                            <A href=compare_href(&[b, id]) attr:class="btn" attr:title="Overlay this run on the previous comparable one">
                                "Compare with previous"
                            </A>
                        }
                    })
            }}
            <button
                class="btn"
                title="The New benchmark modal with this run's row, rung, image, overrides, phases and repetitions"
                disabled=move || run.with(Option::is_none)
                on:click=move |_| {
                    if let Some(r) = run.get_untracked() {
                        bn.open_new(Some(seed_of(&r)));
                    }
                }
            >
                "Benchmark again…"
            </button>
            {del}
        }
    };

    view! {
        <PageFrame
            title=format!("Run {id}")
            sub=Signal::derive(sub)
            class="bench"
            actions=actions
        >
            {move || {
                l.err
                    .get()
                    .map(|e| {
                        view! {
                            <div class="notice err row">
                                {format!("Reading run {id} failed: {e}")}
                                <button class="btn ghost sm" on:click=move |_| l.load()>"Retry"</button>
                            </div>
                        }
                    })
            }}
            {move || live.get().map(|v| view! { <LiveStrip stage=v.stage.clone() done=v.done total=v.total pct=v.percent/> })}
            <Show when=move || run.with(Option::is_some) fallback=move || view! { <div class="empty">"Loading…"</div> }>
                <Body id=id l=l run=run row=row/>
            </Show>
        </PageFrame>
    }
}

/// "Benchmark again": the run's request, overrides included.
fn seed_of(r: &BenchRun) -> BenchArgs {
    let o = &r.settings.overrides;
    BenchArgs {
        model_id: r.model.model_id.clone(),
        rung: r.model.rung,
        image: Some(r.image_ref.clone()).filter(|i| !i.is_empty()),
        ctx_size: o.ctx_size,
        parallel: o.parallel,
        ubatch_size: o.ubatch_size,
        batch_size: o.batch_size,
        cache_type_k: o.cache_type_k.clone(),
        cache_type_v: o.cache_type_v.clone(),
        flash_attn: o.flash_attn.clone(),
        kv_unified: o.kv_unified,
        n_gpu_layers: o.n_gpu_layers,
        no_draft: o.no_draft,
        phases: r.params.phases.clone(),
        repetitions: Some(r.params.repetitions),
        notes: String::new(),
    }
}

#[component]
fn LiveStrip(stage: String, done: u64, total: Option<u64>, pct: Option<u64>) -> impl IntoView {
    let steps = super::steps_words(done, total);
    let tip = stage.clone();
    view! {
        <div class="card bn-live-card">
            <div class="bn-live-line">
                <span class="chip live"><span class="dot"></span>"running"</span>
                <span class="bn-stage" title=tip>{stage}</span>
                <span class="dim">{steps}</span>
                <div class="progress bn-progress" title=pct.map(|p| format!("{p}%"))>
                    <i style=format!("width:{}%", pct.unwrap_or(0))></i>
                </div>
            </div>
            <div class="dim bn-live-note">
                "The GPU is the benchmark's until the run ends: local models answer from their hold fallback or are refused (503 gpu_benchmark). Phases appear below as they finish."
            </div>
        </div>
    }
}

/// What to say before the numbers: how the run ended, what it did not
/// measure, and whether the card was held back.
#[derive(Clone, PartialEq, Default)]
struct Banners {
    status: BenchStatus,
    reason: Option<String>,
    error: Option<String>,
    complete: bool,
    missing: Vec<String>,
    phase_errors: Vec<(String, String)>,
    /// Requests sent a second time after a broken connection (decision 62):
    /// the stage, the request and why the first attempt failed.
    retried: Vec<(String, String, String)>,
    throttled: Vec<&'static str>,
    notes: Vec<String>,
    /// Stored columns that did not decode (decision 51).
    unreadable: Vec<String>,
}

fn banners_of(r: &BenchRun) -> Banners {
    let done = &r.results.phases_done;
    Banners {
        status: r.status,
        reason: r.status_reason.clone(),
        error: r.error.clone(),
        unreadable: r.unreadable.clone(),
        complete: r.results.complete,
        // An unreadable `results` column reads as empty: which phases
        // finished is then unknown, not "none" — its warning says so.
        missing: if r.unreadable.iter().any(|u| u.starts_with("results")) {
            Vec::new()
        } else {
            r.params
                .phases
                .iter()
                .filter(|p| !done.contains(p))
                .map(|p| phase_label(*p).to_string())
                .collect()
        },
        phase_errors: r
            .results
            .phase_errors
            .iter()
            .map(|e| (phase_label(e.phase).to_string(), e.error.clone()))
            .collect(),
        retried: r
            .results
            .retried
            .iter()
            .map(|q| (q.stage.clone(), q.request.clone(), q.error.clone()))
            .collect(),
        throttled: if r.gpu.throttled {
            CLOCK_EVENT_REASONS
                .iter()
                .filter(|(b, _, t)| *t && r.gpu.clock_events & b != 0)
                .map(|(_, n, _)| *n)
                .collect()
        } else {
            Vec::new()
        },
        // A note about a phase the run did not select is not news.
        notes: r
            .results
            .points
            .as_ref()
            .map(|p| p.notes.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|n| {
                let head = n.split(':').next().unwrap_or_default().trim();
                lmgw_api_types::bench::Phase::parse(head)
                    .is_none_or(|p| r.params.phases.contains(&p))
            })
            .chain(
                r.results
                    .mixed
                    .as_ref()
                    .map(|m| m.notes.clone())
                    .unwrap_or_default(),
            )
            .collect(),
    }
}

/// The error sentence the run's end records for an abort (`runner::settle`)
/// — already said by the banner's own words, so not repeated.
fn implied_error(status: BenchStatus, reason: Option<&str>) -> Option<&'static str> {
    match (status, reason) {
        (BenchStatus::Aborted, Some("hold")) => Some("the GPU hold was switched on during the run"),
        (BenchStatus::Aborted, Some("shutdown")) => Some("lmgw was shut down during the run"),
        _ => None,
    }
}

/// The reasons the status banner already explains.
fn explained_reason(status: BenchStatus, reason: &str) -> bool {
    matches!(
        (status, reason),
        (BenchStatus::Canceled, "canceled")
            | (BenchStatus::Aborted, "hold" | "shutdown")
            | (BenchStatus::Interrupted, "shutdown")
    )
}

/// How the run ended, as `(class, head, lines)`, with whatever the row
/// recorded beside its status — an `error` or a `status_reason` — on any
/// run, not only a failed one: a finished run whose container could not be
/// removed carries that in `error` (review finding 8).
fn ended_banner(b: &Banners) -> Option<(&'static str, &'static str, Vec<String>)> {
    let error = b.error.as_deref().filter(|e| !e.trim().is_empty());
    let (class, head, mut lines) = match b.status {
        BenchStatus::Failed => {
            return Some((
                "notice err",
                "The run failed",
                vec![error.unwrap_or("no error was recorded").to_string()],
            ));
        }
        BenchStatus::Canceled => (
            "notice",
            "Canceled",
            vec!["Canceled by the owner. The phases measured before it are kept.".to_string()],
        ),
        BenchStatus::Aborted => (
            "notice warn",
            "Aborted",
            vec![match b.reason.as_deref() {
                Some("hold") => "The GPU hold was switched on during the run, which ends it. The phases measured before are kept.".to_string(),
                Some("shutdown") => "lmgw was quit during the run. The phases measured before are kept.".to_string(),
                _ => "Ended by something other than a cancel. The phases measured before are kept.".to_string(),
            }],
        ),
        BenchStatus::Interrupted => (
            "notice warn",
            "Interrupted",
            vec!["lmgw went away mid-run and found it still running at its next start. The phases stored before are kept.".to_string()],
        ),
        BenchStatus::Done | BenchStatus::Running => ("notice warn", "Recorded with the run", Vec::new()),
    };
    if let Some(e) = error {
        // An abort's own sentence is the banner's; what follows it (a
        // container left behind) is not.
        let implied = implied_error(b.status, b.reason.as_deref());
        let rest: Vec<&str> = e
            .split("; ")
            .filter(|part| Some(part.trim()) != implied)
            .collect();
        if !rest.is_empty() {
            lines.push(rest.join("; "));
        }
    }
    if let Some(r) = b
        .reason
        .as_deref()
        .filter(|r| !explained_reason(b.status, r))
    {
        lines.push(format!("Reason recorded: {r}."));
    }
    (!lines.is_empty()).then_some((class, head, lines))
}

#[component]
fn BannersView(b: Banners) -> impl IntoView {
    let ended = ended_banner(&b);
    let partial =
        (!b.complete && b.status != BenchStatus::Running && !b.missing.is_empty()).then(|| {
            format!(
                "Partial: {} did not finish, so this run is missing {}.",
                b.missing.join(", "),
                if b.missing.len() == 1 {
                    "that phase"
                } else {
                    "those phases"
                }
            )
        });
    let throttled = (!b.throttled.is_empty()).then(|| b.throttled.join(", "));
    view! {
        <div class="bn-banners">
            {ended
                .map(|(class, head, lines)| {
                    view! {
                        <div class=class>
                            <b>{head}</b>
                            {lines.into_iter().map(|l| view! { <span class="detail">{l}</span> }).collect_view()}
                        </div>
                    }
                })}
            {(!b.unreadable.is_empty())
                .then(|| {
                    view! {
                        <div class="notice warn">
                            <b>"Part of this run's stored data could not be read"</b>
                            {b.unreadable.iter().map(|u| view! { <span class="detail mono-sm">{u.clone()}</span> }).collect_view()}
                            <span class="detail dim">
                                "Each unreadable part is shown empty, and the run is compared with nothing: read as empty, it would compare as having measured nothing."
                            </span>
                        </div>
                    }
                })}
            {partial.map(|t| view! { <div class="notice warn"><b>"Partial results"</b>{t}</div> })}
            {(!b.phase_errors.is_empty())
                .then(|| {
                    view! {
                        <div class="notice err">
                            <b>"Phase errors"</b>
                            {b.phase_errors
                                .iter()
                                .map(|(p, e)| view! { <span class="detail">{format!("{p}: {e}")}</span> })
                                .collect_view()}
                        </div>
                    }
                })}
            {(!b.retried.is_empty())
                .then(|| {
                    view! {
                        <div class="notice warn">
                            <b>"Requests sent a second time"</b>
                            {b.retried
                                .iter()
                                .map(|(stage, request, error)| {
                                    view! {
                                        <span class="detail">
                                            {format!("{stage}: {request} lost its connection before any response ({error}).")}
                                        </span>
                                    }
                                })
                                .collect_view()}
                            <span class="detail dim">
                                "Each was sent again at once, and what that step recorded is the second attempt's."
                            </span>
                        </div>
                    }
                })}
            {throttled
                .map(|t| {
                    view! {
                        <div class="notice warn">
                            <b>"The card was held back during this run"</b>
                            {format!("Clock events: {t}. These numbers are throttled numbers: they say what this card did under that limit, not what the build can do.")}
                        </div>
                    }
                })}
            {(!b.notes.is_empty())
                .then(|| {
                    view! {
                        <div class="notice">
                            {b.notes.iter().map(|n| view! { <span class="detail">{n.clone()}</span> }).collect_view()}
                        </div>
                    }
                })}
        </div>
    }
}

#[component]
fn Body(
    id: i64,
    l: Loader,
    run: Memo<Option<BenchRun>>,
    row: RwSignal<Option<LocalModel>>,
) -> impl IntoView {
    let banners = Memo::new(move |_| run.with(|r| r.as_ref().map(banners_of)));
    let headline = Memo::new(move |_| {
        l.data
            .with(|d| d.as_ref().map(|d| d.headline.clone()))
            .unwrap_or_default()
    });
    let cmp = Memo::new(move |_| {
        l.data
            .with(|d| d.as_ref().and_then(|d| d.comparison.clone()))
    });
    let ident = Memo::new(move |_| {
        run.with(|r| {
            r.as_ref().map(|r| {
                let mut r = r.clone();
                // What the identity cards read; the points and samples would
                // redraw them on every tick of a live run.
                r.results.prefill.clear();
                r.results.decode.clear();
                r.results.concurrent.clear();
                r.results.mixed = None;
                r.probes = Default::default();
                r.timeline = Timeline::default();
                r
            })
        })
    });
    let probes = Memo::new(move |_| {
        run.with(|r| {
            r.as_ref()
                .map(|r| r.probes.probes.clone())
                .unwrap_or_default()
        })
    });
    let lines =
        Memo::new(move |_| run.with(|r| r.as_ref().map(line_of).into_iter().collect::<Vec<_>>()));
    let timeline = Memo::new(move |_| run.with(|r| r.as_ref().map(|r| r.timeline.clone())));
    let mixed_foot = Signal::derive(move || {
        run.with(|r| {
            let Some(m) = r.as_ref().and_then(|r| r.results.mixed.as_ref()) else {
                return Vec::new();
            };
            let mut v = vec![
                ("decoding streams".to_string(), m.streams.to_string()),
                (
                    "injected prompt".to_string(),
                    format!("{} tokens", tokens_short(m.inject_tokens)),
                ),
            ];
            if m.stall_ms.median > 0.0 {
                v.push(("stall".into(), fmt_ms(m.stall_ms.median)));
            }
            if m.before_tok_s.median > 0.0 {
                v.push((
                    "rate kept".into(),
                    format!(
                        "{:.0} %",
                        m.during_tok_s.median / m.before_tok_s.median * 100.0
                    ),
                ));
            }
            v
        })
    });
    let energy_foot = Signal::derive(move || {
        run.with(|r| {
            let Some(e) = r.as_ref().map(|r| &r.results.energy) else {
                return Vec::new();
            };
            let mut v = Vec::new();
            if let Some(w) = e.idle_power_w {
                v.push(("idle".to_string(), format!("{w:.0} W")));
            }
            if let Some(w) = e.peak_power_w {
                v.push(("peak".to_string(), format!("{w:.0} W")));
            }
            if let Some(j) = e.total_joules {
                v.push(("whole run".to_string(), format!("{:.1} kJ", j / 1000.0)));
            }
            if let Some(why) = &e.unavailable {
                v.push(("no energy:".to_string(), why.clone()));
            }
            v
        })
    });
    let has_charts = Memo::new(move |_| {
        run.with(|r| {
            r.as_ref().is_some_and(|r| {
                let x = &r.results;
                !x.prefill.is_empty()
                    || !x.decode.is_empty()
                    || !x.concurrent.is_empty()
                    || x.mixed.is_some()
            })
        })
    });
    let probe_count = Signal::derive(move || {
        probes.with(|p| {
            let judged = p
                .iter()
                .filter(|p| {
                    matches!(
                        p.outcome,
                        ProbeOutcome::Pass | ProbeOutcome::Fail | ProbeOutcome::Error
                    )
                })
                .count();
            let passed = p.iter().filter(|p| p.outcome == ProbeOutcome::Pass).count();
            format!("{passed}/{judged} passed")
        })
    });
    let cmp_summary = Signal::derive(move || {
        cmp.with(|c| match c {
            Some(c) => format!("against run {}: {}", c.base_run_id, tally(c)),
            None if run.with(|r| r.as_ref().is_some_and(|r| !r.unreadable.is_empty())) => {
                "not compared: unreadable stored data".into()
            }
            None => "no earlier comparable run".into(),
        })
    });
    let stored_notes =
        run.with_untracked(|r| r.as_ref().map(|r| r.notes.clone()).unwrap_or_default());
    view! {
        {move || banners.get().map(|b| view! { <BannersView b=b/> })}
        {move || view! { <HeadlineTiles h=headline.get() cmp=cmp.get()/> }}
        <Section title="Comparison" summary=cmp_summary persist="bench.detail.compare">
            {move || {
                let unreadable = run.with(|r| r.as_ref().is_some_and(|r| !r.unreadable.is_empty()));
                view! { <Comparison id=id cmp=cmp.get() unreadable=unreadable/> }
            }}
        </Section>
        <Section title="Probes" count=probe_count summary=probe_count persist="bench.detail.probes">
            {move || view! { <div class="card pad0"><ProbesList probes=probes.get()/></div> }}
        </Section>
        <Section title="Phases" persist="bench.detail.charts">
            <Show
                when=move || has_charts.get()
                fallback=|| view! { <div class="empty">"This run measured no throughput phase."</div> }
            >
                <PhaseCharts lines=lines mixed_foot=mixed_foot energy_foot=energy_foot/>
            </Show>
            <TimelineCard timeline=timeline/>
        </Section>
        <Section title="What ran" persist="bench.detail.identity">
            {move || ident.get().map(|r| view! { <IdentityCards run=r row=row.get()/> })}
            <NotesEditor id=id stored=stored_notes.clone()/>
        </Section>
    }
}

#[component]
fn Comparison(
    id: i64,
    cmp: Option<BenchComparison>,
    /// Part of the run's stored data did not decode: it is compared with
    /// nothing (decision 51), whatever ran before it.
    unreadable: bool,
) -> impl IntoView {
    let Some(c) = cmp else {
        let why = if unreadable {
            "Not compared: part of this run's stored data could not be read, so it is compared with nothing."
        } else {
            "No earlier comparable run: none that finished with the same weights, settings, suite version and GPU. Runs of other builds of the same row compare — that is what comparing builds is — so a second run with another image lands here."
        };
        return view! {
            <div class="card bn-cmp-none">
                <p class="dim">{why}</p>
                <ThresholdControl/>
            </div>
        }
        .into_any();
    };
    let base = c.base_run_id;
    let head = format!(
        "Against run {base}, {} ({})",
        ago(&c.base_created_at),
        tally(&c)
    );
    view! {
        <div class="card edit-section bn-cmp">
            <div class="cmd-head">
                <span class="bn-cmp-head">{head}</span>
                <A href=compare_href(&[base, id]) attr:class="btn sm">"Overlay the two"</A>
                <span class="spacer"></span>
                <ThresholdControl/>
            </div>
            {(!c.comparable)
                .then(|| {
                    view! {
                        <div class="notice warn">
                            <b>"Not comparable"</b>
                            {c.not_comparable.join("; ")}
                        </div>
                    }
                })}
            <DeltaTable cmp=c base=format!("run {base}") this=format!("run {id}")/>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(status: BenchStatus, reason: Option<&str>, error: Option<&str>) -> Banners {
        Banners {
            status,
            reason: reason.map(str::to_string),
            error: error.map(str::to_string),
            ..Default::default()
        }
    }

    /// Review finding 8: a finished run whose container could not be
    /// removed had its note only in the runs table's tooltip.
    #[test]
    fn an_error_shows_on_any_run_and_an_implied_one_is_not_repeated() {
        let left = "the bench container lmgw-bench-7 could not be removed (busy)";
        let (class, head, lines) = ended_banner(&b(BenchStatus::Done, None, Some(left))).unwrap();
        assert_eq!((class, head), ("notice warn", "Recorded with the run"));
        assert_eq!(lines, vec![left.to_string()]);
        assert!(ended_banner(&b(BenchStatus::Done, None, None)).is_none());

        let (_, _, lines) =
            ended_banner(&b(BenchStatus::Canceled, Some("canceled"), Some(left))).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[1], left);

        // The abort's own sentence is the banner's; the note after it is not.
        let e = format!("the GPU hold was switched on during the run; {left}");
        let (_, head, lines) =
            ended_banner(&b(BenchStatus::Aborted, Some("hold"), Some(&e))).unwrap();
        assert_eq!(head, "Aborted");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[1], left);
        let (_, _, lines) = ended_banner(&b(
            BenchStatus::Aborted,
            Some("hold"),
            Some("the GPU hold was switched on during the run"),
        ))
        .unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");

        // A reason the banner does not explain is named.
        let (_, _, lines) = ended_banner(&b(BenchStatus::Done, Some("odd"), None)).unwrap();
        assert_eq!(lines, vec!["Reason recorded: odd.".to_string()]);
        let (_, head, lines) = ended_banner(&b(BenchStatus::Failed, None, None)).unwrap();
        assert_eq!(
            (head, lines[0].as_str()),
            ("The run failed", "no error was recorded")
        );
    }
}
