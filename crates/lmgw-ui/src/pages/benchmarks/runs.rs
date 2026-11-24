//! The runs list (benchmark design §8.2): one row per run, newest first —
//! the running one on top with its stage and progress off the jobs feed —
//! named by its row, its state (a dot and a word), and a line of facts: its
//! id, rung and quant, the build, the headline speeds, its verdict against
//! the previous comparable run, and its age. Every number is in the opened
//! run's detail and in the row's tooltip.
//!
//! Compare: the header's pick button turns a click into a pick, in the
//! order picked; the first one is the base. The foot then says how many
//! and opens the comparison.
//!
//! With nothing opened the detail is the status page: what a run measures,
//! the run going now, the threshold and New benchmark.

use leptos::prelude::*;
use lmgw_api_types::bench_ops::BenchRunSummary;

use super::delta::ThresholdControl;
use super::frame::BnFrame;
use super::{build_words, compare_href, fmt_rate, run_href, status_look, use_bn, Bn};
use crate::bench_api::LiveBench;
use crate::pages::backends::{ago, local_ts};
use crate::url_state::use_query_signal;
use crate::widgets::list_pane::{ListBox, ListHead, ListNav, ListRefs, ListSearch};
use crate::widgets::{filter_words, matches_word, Dot, Ico, Icon, ListRow, PrefGroup, StackRow};

/// The list pane's state, made by the page.
#[derive(Clone, Copy)]
pub struct RunsList {
    pub nav: ListNav<i64>,
    bn: Bn,
    query: RwSignal<String>,
    /// A click picks for Compare instead of opening.
    picking: RwSignal<bool>,
    /// The picks, in the order picked: the first is the base.
    picked: RwSignal<Vec<i64>>,
    shown: Memo<Vec<i64>>,
}

fn matches(s: &BenchRunSummary, q: &str) -> bool {
    let hay = format!(
        "#{} {} {} {} {} {}",
        s.id,
        s.model_id,
        s.quant.as_deref().unwrap_or(""),
        s.image_ref,
        s.engine_slug.as_deref().unwrap_or(""),
        s.notes
    );
    filter_words(q).iter().all(|w| matches_word(&hay, w))
}

impl RunsList {
    pub fn new(
        bn: Bn,
        sel: Memo<Option<i64>>,
        narrow: Memo<bool>,
        open: Callback<(i64, bool)>,
    ) -> Self {
        let query = use_query_signal("q");
        let shown = Memo::new(move |_| {
            let q = query.get();
            bn.runs.with(|r| {
                r.as_ref()
                    .map(|r| {
                        r.runs
                            .iter()
                            .filter(|s| matches(s, &q))
                            .map(|s| s.id)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
        });
        let picked = RwSignal::new(Vec::<i64>::new());
        // A pick whose run went away is dropped.
        Effect::new(move |_| {
            let ids = bn.runs.with(|r| {
                r.as_ref()
                    .map(|r| r.runs.iter().map(|s| s.id).collect::<Vec<_>>())
                    .unwrap_or_default()
            });
            if picked.with_untracked(|p| p.iter().any(|id| !ids.contains(id))) {
                picked.update(|p| p.retain(|id| ids.contains(id)));
            }
        });
        let nav = ListNav::new(ListRefs::new(), shown, sel, narrow, open, |id| {
            format!("/benchmarks/{id}")
        });
        Self {
            nav,
            bn,
            query,
            picking: RwSignal::new(false),
            picked,
            shown,
        }
    }

    fn toggle_pick(self, id: i64) {
        self.picked.update(|p| {
            if let Some(i) = p.iter().position(|x| *x == id) {
                p.remove(i);
            } else {
                p.push(id);
            }
        });
    }
}

/// A run's state as the list says it: its phase and progress while it
/// runs, else how it ended.
fn state_of(s: &BenchRunSummary, live: Option<&LiveBench>) -> (Dot, String) {
    if let Some(l) = live {
        let words = match l.percent {
            Some(p) => format!("{} {p}%", l.phase_word()),
            None => l.phase_word(),
        };
        return (Dot::Busy, words);
    }
    let (dot, words, _) = status_look(
        s.status,
        s.complete,
        s.throttled,
        s.status_reason.as_deref(),
        s.error.as_deref(),
    );
    let words = if s.unreadable.is_empty() {
        words
    } else {
        format!("{words}, unreadable")
    };
    (dot, words)
}

/// The row's line of facts.
fn meta_of(s: &BenchRunSummary, live: Option<&LiveBench>) -> String {
    let mut parts = vec![format!("#{}", s.id)];
    if s.rung > 0 {
        parts.push(format!("rung {}", s.rung));
    }
    if let Some(q) = &s.quant {
        parts.push(q.clone());
    }
    if let Some(l) = live {
        parts.push(l.stage.clone());
        let steps = super::steps_words(l.done, l.total);
        if !steps.is_empty() {
            parts.push(steps);
        }
        return parts.join(" · ");
    }
    parts.push(build_words(
        &s.image_ref,
        s.engine_slug.as_deref(),
        s.version.as_deref(),
        s.commit.as_deref(),
    ));
    if let Some(v) = s.headline.prefill_tok_s {
        parts.push(format!("prefill {}", fmt_rate(v)));
    }
    if let Some(v) = s.headline.decode_tok_s {
        parts.push(format!("decode {}", fmt_rate(v)));
    }
    if s.previous_id.is_some() {
        let (r, i) = (s.regressions.unwrap_or(0), s.improvements.unwrap_or(0));
        match (r, i) {
            (0, 0) => parts.push("same as before".into()),
            (r, 0) => parts.push(format!("▼ {r}")),
            (0, i) => parts.push(format!("▲ {i}")),
            (r, i) => parts.push(format!("▼ {r} ▲ {i}")),
        }
    }
    parts.push(ago(&s.created_at));
    parts.join(" · ")
}

/// The row's tooltip: everything its two lines leave out.
fn tip_of(s: &BenchRunSummary, threshold: f64) -> String {
    let (_, words, why) = status_look(
        s.status,
        s.complete,
        s.throttled,
        s.status_reason.as_deref(),
        s.error.as_deref(),
    );
    let mut t = vec![
        format!("Run {} of {}", s.id, s.model_id),
        format!("{words}: {why}"),
        format!(
            "started {}{}",
            local_ts(&s.created_at),
            s.finished_at
                .as_deref()
                .map(|f| format!(", ended {}", local_ts(f)))
                .unwrap_or_default()
        ),
        s.image_ref.clone(),
    ];
    let h = &s.headline;
    let mut nums = Vec::new();
    if let (Some(v), Some(t)) = (h.prefill_tok_s, h.prefill_tokens) {
        nums.push(format!("prefill {} tok/s at {t} tokens", fmt_rate(v)));
    }
    if let Some(v) = h.ttft_ms {
        nums.push(format!("TTFT {}", super::fmt_ms(v)));
    }
    if let Some(v) = h.decode_tok_s {
        nums.push(format!("decode {} tok/s at depth 64", fmt_rate(v)));
    }
    if let (Some(v), Some(n)) = (h.aggregate_tok_s, h.aggregate_streams) {
        nums.push(format!("aggregate {} tok/s with {n} streams", fmt_rate(v)));
    }
    if let Some(v) = h.decode_tokens_per_joule {
        nums.push(format!("{} tok/J", super::fmt_tokj(v)));
    }
    if let Some(v) = h.load_ms {
        nums.push(format!("load {}", super::fmt_ms(v as f64)));
    }
    if let Some(v) = h.load_vram_bytes {
        nums.push(format!("VRAM {}", crate::fmt::fmt_bytes(v)));
    }
    if !nums.is_empty() {
        t.push(nums.join(" · "));
    }
    if s.probes_judged > 0 {
        t.push(format!(
            "{} of {} probes with a verdict passed",
            s.probes_passed, s.probes_judged
        ));
    }
    if let Some(prev) = s.previous_id {
        t.push(format!(
            "against run {prev} at a {threshold} % threshold: {} worse, {} better",
            s.regressions.unwrap_or(0),
            s.improvements.unwrap_or(0)
        ));
    }
    if !s.unreadable.is_empty() {
        t.push(format!(
            "Part of this run's stored data could not be read, so it is compared with nothing: {}",
            s.unreadable.join(", ")
        ));
    }
    if !s.notes.is_empty() {
        t.push(s.notes.clone());
    }
    t.join("\n")
}

#[component]
pub fn RunsPane(list: RunsList, narrow: Memo<bool>) -> impl IntoView {
    let bn = list.bn;
    let nav = list.nav;
    let sub = Signal::derive(move || {
        let total = bn.runs.with(|r| r.as_ref().map(|r| r.runs.len()));
        match total {
            None if bn.runs_err.with(Option::is_some) => "not loaded".to_string(),
            None => "Loading…".to_string(),
            Some(n) => {
                let all = crate::fmt::count_of(n, "runs");
                if list.query.with(|q| q.trim().is_empty()) {
                    all
                } else {
                    format!("{} of {all}", list.shown.with(Vec::len))
                }
            }
        }
    });
    let actions = ViewFn::from(move || {
        view! {
            <button
                type="button"
                class="icon-btn bn-pick-btn"
                class:on=move || list.picking.get()
                aria-pressed=move || list.picking.get().to_string()
                title="Pick runs to compare: a click picks a run instead of opening it; the first one picked is the base"
                aria-label="Pick runs to compare"
                on:click=move |_| list.picking.update(|p| *p = !*p)
            >
                <Icon ico=Ico::Layers/>
            </button>
            <button
                type="button"
                class="icon-btn"
                title="New benchmark: pick a row, an image and the phases, see the plan, then start"
                aria-label="New benchmark"
                on:click=move |_| bn.open_new(None)
            >
                <Icon ico=Ico::Plus/>
            </button>
        }
    });
    let threshold = Memo::new(move |_| {
        bn.runs
            .with(|r| r.as_ref().map(|r| r.threshold_pct))
            .unwrap_or_else(|| bn.threshold.get())
    });
    view! {
        <ListHead scrolled=nav.scrolled narrow=narrow title="Benchmarks" sub=sub actions=actions/>
        <ListSearch
            nav=nav
            query=list.query
            placeholder="Search runs"
            title="Run id, model, quant, image or notes"
        />
        {move || {
            bn.runs_err
                .get()
                .map(|e| {
                    view! {
                        <div class="notice err mlist-notice">
                            "Loading the runs failed: "
                            {e}
                            <button class="btn sm" on:click=move |_| bn.load_runs()>
                                "Retry"
                            </button>
                        </div>
                    }
                })
        }}
        <ListBox nav=nav label="Benchmark runs">
            <Show when=move || bn.runs.with(Option::is_none) && bn.runs_err.with(Option::is_none)>
                <div class="mlist-empty">"Loading…"</div>
            </Show>
            <Show when=move || bn.runs.with(Option::is_some) && list.shown.with(Vec::is_empty)>
                <div class="mlist-empty">
                    {move || {
                        let q = list.query.get();
                        if q.trim().is_empty() {
                            "No benchmark runs yet: start one with +".to_string()
                        } else {
                            format!("No runs match \u{201c}{}\u{201d}", q.trim())
                        }
                    }}
                </div>
            </Show>
            <For each=move || list.shown.get() key=|id| *id let:id>
                <RunRow id=id list=list threshold=threshold/>
            </For>
        </ListBox>
        <Show when=move || list.picking.get() || !list.picked.with(Vec::is_empty)>
            <div class="mlist-foot bn-pick-foot">
                <button
                    type="button"
                    class="btn sm suggested"
                    disabled=move || list.picked.with(Vec::len) < 2
                    title="Overlay the picked runs; the first one picked is the base the others are judged against"
                    on:click=move |_| bn.go(compare_href(&list.picked.get_untracked()))
                >
                    {move || match list.picked.with(Vec::len) {
                        0 | 1 => "Compare picked runs".to_string(),
                        k => format!("Compare {k} runs"),
                    }}
                </button>
                <span class="bn-pick-note">
                    {move || {
                        list.picked
                            .with(|p| p.first().copied())
                            .map(|b| format!("base: run {b}"))
                            .unwrap_or_else(|| "Click runs to pick them".into())
                    }}
                </span>
                <button
                    type="button"
                    class="link-btn"
                    on:click=move |_| {
                        list.picked.set(Vec::new());
                        list.picking.set(false);
                    }
                >
                    "Done"
                </button>
            </div>
        </Show>
    }
}

#[component]
fn RunRow(id: i64, list: RunsList, threshold: Memo<f64>) -> impl IntoView {
    let bn = list.bn;
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
    let st = Memo::new(move |_| live.with(|l| s.with(|s| state_of(s, l.as_ref()))));
    let pick_at = Memo::new(move |_| list.picked.with(|p| p.iter().position(|x| *x == id)));
    let state = move || {
        if list.picking.get() {
            match pick_at.get() {
                Some(0) => return "base".to_string(),
                Some(i) => return format!("pick {}", i + 1),
                None => {}
            }
        }
        st.get().1
    };
    view! {
        <ListRow
            key=format!("/benchmarks/{id}")
            name=move || s.with(|s| s.model_id.clone())
            meta=move || live.with(|l| s.with(|s| meta_of(s, l.as_ref())))
            title=move || s.with(|s| tip_of(s, threshold.get()))
            dot=Signal::derive(move || st.get().0)
            state=state
            selected=Signal::derive(move || {
                if list.picking.get() || !list.picked.with(Vec::is_empty) {
                    pick_at.get().is_some()
                } else {
                    list.nav.lit(&id)
                }
            })
            on_click=Callback::new(move |()| {
                if list.picking.get_untracked() {
                    list.toggle_pick(id);
                } else {
                    list.nav.click(id);
                }
            })
        />
    }
}

/// Nothing opened: what a run measures, the run going now, the threshold.
#[component]
pub fn StatusPage() -> impl IntoView {
    let bn = use_bn();
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
    let actions = ViewFn::from(move || {
        view! {
            <button
                type="button"
                class="btn suggested"
                title="Pick a row, an image and the phases, see the plan, then start"
                on:click=move |_| bn.open_new(None)
            >
                "New benchmark"
            </button>
        }
    });
    view! {
        <BnFrame title="Benchmarks" sub=Signal::derive(sub) actions=actions>
            {move || {
                bn.live
                    .get()
                    .map(|l| {
                        let href = run_href(l.run_id);
                        let steps = super::steps_words(l.done, l.total);
                        let pct = l.percent;
                        view! {
                            <PrefGroup title="Running now">
                                <a class="prow action" href=href>
                                    <div class="prow-text">
                                        <span class="prow-title">
                                            {format!("Run {} · {}", l.run_id, l.model_id)}
                                        </span>
                                        <span class="prow-sub">{format!("{} {steps}", l.stage)}</span>
                                    </div>
                                    <div class="progress bn-progress" title=pct.map(|p| format!("{p}%"))>
                                        <i style=format!("width:{}%", pct.unwrap_or(0))></i>
                                    </div>
                                    <Icon ico=Ico::Right class="prow-chevron"/>
                                </a>
                            </PrefGroup>
                        }
                    })
            }}
            <PrefGroup title="What a run measures">
                <StackRow>
                    <p class="bn-about">
                        "A run measures one local chat row on one llama-server image: load time and VRAM, "
                        "whether the knobs lmgw relies on work (thinking on and off, tool calls, JSON schema, "
                        "vision), prefill and decode speed across the row's whole context, concurrent and "
                        "mixed load, and tokens per joule. It needs the whole GPU while it runs."
                    </p>
                </StackRow>
            </PrefGroup>
            <PrefGroup
                title="Comparison"
                desc="A run is judged against the previous comparable one: the same weights, settings, suite and GPU. Pick runs on the list (the layers button) to overlay any of them."
            >
                <div class="prow bn-thr-row">
                    <ThresholdControl/>
                </div>
            </PrefGroup>
        </BnFrame>
    }
}

#[cfg(test)]
mod tests {
    use lmgw_api_types::bench_ops::BenchStatus;

    use super::*;

    fn run(status: BenchStatus, complete: bool, throttled: bool) -> BenchRunSummary {
        BenchRunSummary {
            status,
            complete,
            throttled,
            ..Default::default()
        }
    }

    #[test]
    fn list_dot_and_hero_chip_agree_for_every_ending() {
        for (s, dot) in [
            (run(BenchStatus::Done, true, false), Dot::Stopped),
            (run(BenchStatus::Done, true, true), Dot::Warn),
            (run(BenchStatus::Done, false, false), Dot::Warn),
            (run(BenchStatus::Failed, false, false), Dot::Error),
            (run(BenchStatus::Canceled, false, false), Dot::Off),
            (run(BenchStatus::Aborted, false, false), Dot::Warn),
            (run(BenchStatus::Running, false, false), Dot::Busy),
        ] {
            let hero = status_look(s.status, s.complete, s.throttled, None, None).0;
            assert_eq!(state_of(&s, None).0, dot);
            assert_eq!(hero, dot);
        }
    }
}
