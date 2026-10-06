//! Benchmarks (benchmark design §8.2): runs of the benchmark suite against a
//! local chat row, an image and a rung — what the owner reads to choose
//! between rows, quants, builds and settings.
//!
//! One route, `/benchmarks`, and three views the query string picks:
//!
//! * bare — the runs table ([`runs`]), newest first, the running one on top;
//! * `?run=<id>` — one run whole ([`detail`], [`identity`], [`probes`], the
//!   charts), with its comparison against the previous comparable run;
//! * `?compare=<id>,<id>,…` — runs overlaid ([`compare`]), judged against the
//!   first one with the ops' own rule (`lmgw_api_types::bench_compare`).
//!
//! `?new=1` (with `&model=<id>` and `&image=<ref>` from the Models and
//! Backends entry points) opens the New benchmark modal ([`new_run`]) over
//! whichever view is shown.
//!
//! Nothing polls on a timer. The runs list is re-read when a run starts or
//! ends (the `benchmark` job appears on or leaves the jobs feed), when the
//! threshold changes and after a change to a run; a running run's detail is
//! re-read on each progress tick of its job.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;
use lmgw_api_types::bench::Phase;
use lmgw_api_types::bench_ops::{BenchArgs, BenchRunsResponse, BenchStatus, DEFAULT_THRESHOLD_PCT};

use crate::bench_api::{self as api, LiveBench};
use crate::scope::{Latest, Scope};
use crate::url_state::use_query_signal;
use crate::widgets::{use_toasts, Toasts};

mod charts;
mod compare;
mod delta;
mod detail;
mod identity;
mod new_run;
mod notes;
mod phase_charts;
mod plan_view;
mod probes;
mod runs;
mod timeline;

// ---------------------------------------------------------------------------
// Page state
// ---------------------------------------------------------------------------

/// State every view, row and modal of the page reaches. `Copy`.
#[derive(Clone, Copy)]
pub struct Bn {
    pub toasts: Toasts,
    pub scope: Scope,
    pub runs: RwSignal<Option<BenchRunsResponse>>,
    pub runs_err: RwSignal<Option<String>>,
    runs_read: Latest,
    /// The run going now, off the jobs feed.
    pub live: Memo<Option<LiveBench>>,
    /// The regression threshold as typed, kept as `lmgw.ui.bench.threshold`.
    pub threshold_text: RwSignal<String>,
    /// … as a percentage: the typed one when it is one, else the default.
    pub threshold: Memo<f64>,
    /// Bumped after a change to a stored run (notes, a delete): an open
    /// detail or comparison reads its runs again.
    pub rev: RwSignal<u64>,
    pub q_run: RwSignal<String>,
    pub q_compare: RwSignal<String>,
    pub q_new: RwSignal<String>,
    pub q_model: RwSignal<String>,
    pub q_image: RwSignal<String>,
    /// A whole request to open the modal with ("Benchmark again"), beyond
    /// what `?model=` and `?image=` carry.
    pub seed: RwSignal<Option<BenchArgs>>,
    /// The run whose notes the notes modal edits, with them.
    pub notes: RwSignal<Option<(i64, String)>>,
    nav: Callback<String>,
}

impl Bn {
    /// Read the runs again. Safe from any continuation: once the page is
    /// gone there is nothing to load.
    pub fn load_runs(self) {
        if !self.scope.alive() {
            return;
        }
        let Some(ticket) = self.runs_read.next() else {
            return;
        };
        let threshold = self.threshold.get_untracked();
        self.scope.spawn(async move {
            let res = api::runs(threshold).await;
            if !self.runs_read.is(ticket) {
                return;
            }
            match res {
                Ok(r) => {
                    self.runs_err.set(None);
                    self.runs.set(Some(r));
                }
                Err(e) => self.runs_err.set(Some(e.to_string())),
            }
        });
    }

    /// A link inside the app (history entry, no reload).
    pub fn go(self, href: String) {
        self.nav.run(href);
    }

    /// Open the New benchmark modal, prefilled with `seed` when there is one.
    pub fn open_new(self, seed: Option<BenchArgs>) {
        self.seed.set(seed);
        self.q_new.set("1".into());
    }

    pub fn cancel(self, run_id: i64) {
        let toasts = self.toasts;
        spawn_local(async move {
            match api::cancel(run_id).await {
                Ok(d) => toasts.ok(d.message),
                Err(e) => toasts.err(format!("run {run_id}: {e}")),
            }
        });
    }

    /// Delete a finished run; `then` runs once it is gone (the detail goes
    /// back to the list).
    pub fn delete(self, id: i64, then: Option<Callback<()>>) {
        let toasts = self.toasts;
        spawn_local(async move {
            match api::delete(id).await {
                Ok(d) => {
                    toasts.ok(d.message);
                    if !self.scope.alive() {
                        return;
                    }
                    self.rev.update(|r| *r += 1);
                    self.load_runs();
                    if let Some(t) = then {
                        t.run(());
                    }
                }
                Err(e) => toasts.err(format!("run {id}: {e}")),
            }
        });
    }
}

pub fn use_bn() -> Bn {
    expect_context::<Bn>()
}

/// A threshold the owner typed, when it is a percentage.
pub fn parse_threshold(t: &str) -> Option<f64> {
    let v = t.trim().trim_end_matches('%').trim().parse::<f64>().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// The ids of `?compare=`, in the order given, without repeats.
pub fn parse_ids(s: &str) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for id in s.split(',').filter_map(|p| p.trim().parse::<i64>().ok()) {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

pub fn run_href(id: i64) -> String {
    format!("/benchmarks?run={id}")
}

pub fn compare_href(ids: &[i64]) -> String {
    let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
    format!("/benchmarks?compare={}", ids.join(","))
}

/// Where the Models and Backends entry points send the owner: the modal,
/// prefilled with the row or the image.
pub fn new_href(model: Option<&str>, image: Option<&str>) -> String {
    let mut h = "/benchmarks?new=1".to_string();
    for (k, v) in [("model", model), ("image", image)] {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            h.push_str(&format!("&{k}={}", query_value(v)));
        }
    }
    h
}

/// A query-string value: what would end or split it is escaped; `/` and `:`
/// stay readable (an image reference is full of them).
fn query_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Words and numbers
// ---------------------------------------------------------------------------

/// Tokens per second: whole above 100 ("12 202"), one decimal below.
pub fn fmt_rate(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    if v == 0.0 {
        return "0".into();
    }
    if v.abs() >= 100.0 {
        crate::fmt::grouped(v.round().max(0.0) as u64)
    } else if v.abs() >= 10.0 {
        format!("{v:.1}")
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Milliseconds as a person reads them: "41 ms", "3.09 s", "20.2 s".
pub fn fmt_ms(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    if v < 1.0 && v > 0.0 {
        format!("{v:.2} ms")
    } else if v < 1000.0 {
        format!("{v:.0} ms")
    } else if v < 10_000.0 {
        format!("{:.2} s", v / 1000.0)
    } else if v < 120_000.0 {
        format!("{:.1} s", v / 1000.0)
    } else {
        let s = (v / 1000.0).round() as u64;
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

/// A millisecond tick on a chart's axis: as short as it can be ("100 ms",
/// "2 s", "1.5 s").
pub fn fmt_ms_axis(v: f64) -> String {
    if v >= 1000.0 {
        let s = v / 1000.0;
        if (s - s.round()).abs() < 1e-9 {
            format!("{s:.0} s")
        } else {
            format!("{s:.1} s")
        }
    } else if v >= 1.0 || v == 0.0 {
        format!("{v:.0} ms")
    } else {
        format!("{v:.2} ms")
    }
}

/// Tokens per joule: three significant digits at most, no trailing zeros
/// ("1.66", "4", "42.5").
pub fn fmt_tokj(v: f64) -> String {
    let s = if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    };
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// A token count on an axis: 512, "2k", "128k", "256k" (binary thousands,
/// which is what the suite's lengths are made of).
pub fn tokens_short(v: u64) -> String {
    if v >= 1024 {
        format!("{}k", (v as f64 / 1024.0).round() as u64)
    } else {
        v.to_string()
    }
}

/// A live run's progress in steps; nothing before the first step is
/// counted (the setup stages carry no count).
pub fn steps_words(done: u64, total: Option<u64>) -> String {
    match (done, total) {
        (d, Some(t)) if t > 0 => format!("step {d} of {t}"),
        (0, _) => String::new(),
        (d, _) => format!("step {d}"),
    }
}

/// A signed percentage with a real minus: "+3.2 %", "−15.2 %".
pub fn fmt_delta(v: f64) -> String {
    if v.abs() < 0.05 {
        return "±0.0 %".into();
    }
    let sign = if v > 0.0 {
        "+"
    } else if v < 0.0 {
        "\u{2212}"
    } else {
        "±"
    };
    format!("{sign}{:.1} %", v.abs())
}

/// A headline value in its unit, as the delta table and the tiles show it.
pub fn fmt_unit(unit: &str, v: f64) -> String {
    match unit {
        "tok/s" => format!("{} tok/s", fmt_rate(v)),
        "ms" => fmt_ms(v),
        "bytes" => crate::fmt::human_bytes(v.max(0.0) as u64),
        "tok/J" => format!("{} tok/J", fmt_tokj(v)),
        other => format!("{v:.2} {other}"),
    }
}

pub fn phase_label(p: Phase) -> &'static str {
    match p {
        Phase::Load => "load",
        Phase::Probes => "probes",
        Phase::Prefill => "prefill",
        Phase::Decode => "decode",
        Phase::Concurrent => "concurrent",
        Phase::Mixed => "mixed",
    }
}

/// What a build is in a few words: the engine slug, then the version or the
/// commit — whichever the image carries — else the image's tag.
pub fn build_words(
    image_ref: &str,
    slug: Option<&str>,
    version: Option<&str>,
    commit: Option<&str>,
) -> String {
    let head = slug
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::pages::backends::tag_only(image_ref).to_string());
    let tail = version
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| {
            commit
                .filter(|c| !c.is_empty())
                .map(crate::pages::backends::sha7)
        });
    match tail {
        Some(t) if !head.contains(&t) => format!("{head} · {t}"),
        _ => head,
    }
}

/// The status chip of a run: `(class, words, tooltip)`. A done run that
/// did not finish every phase is partial; a throttled one says so, because
/// its numbers are throttled numbers.
pub fn status_look(
    status: BenchStatus,
    complete: bool,
    throttled: bool,
    reason: Option<&str>,
    error: Option<&str>,
) -> (&'static str, String, String) {
    let why = |t: &str| {
        let mut s = t.to_string();
        if let Some(e) = error.filter(|e| !e.is_empty()) {
            s.push_str(&format!("\n{e}"));
        }
        if throttled {
            s.push_str("\nThe card throttled during the run: these are throttled numbers.");
        }
        s
    };
    match status {
        BenchStatus::Running => ("chip live", "running".into(), why("running now")),
        BenchStatus::Done if !complete => (
            "chip warn",
            "partial".into(),
            why("finished, but not every selected phase completed"),
        ),
        BenchStatus::Done if throttled => (
            "chip warn",
            "done, throttled".into(),
            why("every selected phase completed"),
        ),
        BenchStatus::Done => (
            "chip ok",
            "done".into(),
            why("every selected phase completed"),
        ),
        BenchStatus::Failed => ("chip err", "failed".into(), why("a phase failed")),
        BenchStatus::Canceled => ("chip off", "canceled".into(), why("canceled by the owner")),
        BenchStatus::Aborted => (
            "chip warn",
            match reason {
                Some("hold") => "aborted: hold".into(),
                Some("shutdown") => "aborted: quit".into(),
                _ => "aborted".into(),
            },
            why(match reason {
                Some("hold") => "the GPU hold was switched on during the run",
                Some("shutdown") => "lmgw was quit during the run",
                _ => "ended by something other than a cancel",
            }),
        ),
        BenchStatus::Interrupted => (
            "chip warn",
            "interrupted".into(),
            why("lmgw went away mid-run; found still running at the next start"),
        ),
    }
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq)]
enum View {
    Runs,
    Run(i64),
    Compare(Vec<i64>),
}

#[component]
pub fn Benchmarks() -> impl IntoView {
    let feed = crate::live::use_live();
    let live = Memo::new(move |_| {
        feed.jobs
            .with(|j| api::live(j.as_deref().unwrap_or_default()))
    });
    let threshold_text = crate::prefs::persisted_string("bench.threshold", "5");
    let threshold =
        Memo::new(move |_| parse_threshold(&threshold_text.get()).unwrap_or(DEFAULT_THRESHOLD_PCT));
    let navigate = use_navigate();
    let bn = Bn {
        toasts: use_toasts(),
        scope: Scope::new(),
        runs: RwSignal::new(None),
        runs_err: RwSignal::new(None),
        runs_read: Latest::new(),
        live,
        threshold_text,
        threshold,
        rev: RwSignal::new(0),
        q_run: use_query_signal("run"),
        q_compare: use_query_signal("compare"),
        q_new: use_query_signal("new"),
        q_model: use_query_signal("model"),
        q_image: use_query_signal("image"),
        seed: RwSignal::new(None),
        notes: RwSignal::new(None),
        nav: Callback::new(move |href: String| navigate(&href, Default::default())),
    };
    provide_context(bn);
    bn.load_runs();

    // A run starting or ending is when the list changes; so is the
    // threshold its badges are judged at.
    let live_job = Memo::new(move |_| live.with(|l| l.as_ref().map(|l| l.job_id)));
    Effect::new(move |prev: Option<(Option<i64>, u64)>| {
        let now = (live_job.get(), threshold.get().to_bits());
        if prev.is_some_and(|p| p != now) {
            bn.load_runs();
        }
        now
    });

    let view = Memo::new(move |_| {
        if let Ok(id) = bn.q_run.get().trim().parse::<i64>() {
            return View::Run(id);
        }
        let ids = parse_ids(&bn.q_compare.get());
        if !ids.is_empty() {
            return View::Compare(ids);
        }
        View::Runs
    });

    view! {
        {move || match view.get() {
            View::Runs => view! { <runs::RunsView/> }.into_any(),
            View::Run(id) => view! { <detail::DetailView id=id/> }.into_any(),
            View::Compare(ids) => view! { <compare::CompareView ids=ids/> }.into_any(),
        }}
        // The page's own modals, under the page's density: a `<dialog>` in
        // the top layer still inherits its custom properties from here.
        <div class="density-dense bn-modals">
            <new_run::NewRunModal/>
            <notes::NotesModal/>
        </div>
    }
}

/// The sidebar's Benchmarks badge: 1 while a run is going (there is at most
/// one), with its stage in the tooltip.
pub fn nav_badge(live: crate::live::LiveBus) -> (Signal<u64>, Signal<String>) {
    let bench = Memo::new(move |_| {
        live.jobs
            .with(|j| api::live(j.as_deref().unwrap_or_default()))
    });
    let badge = Signal::derive(move || u64::from(bench.with(Option::is_some)));
    let title = Signal::derive(move || {
        bench.with(|b| {
            b.as_ref()
                .map(|b| {
                    format!(
                        "benchmark run {} of {} running: {}",
                        b.run_id, b.model_id, b.stage
                    )
                })
                .unwrap_or_default()
        })
    });
    (badge, title)
}

/// The sidebar GPU pop's line while a run holds the card (§3.2): which run,
/// what that means for local models, and the way to it.
#[component]
pub fn GpuBenchRow(open: RwSignal<bool>) -> impl IntoView {
    let feed = crate::live::use_live();
    let bench = Memo::new(move |_| {
        feed.vram
            .with(|v| v.as_ref().and_then(|v| v.benchmark.clone()))
    });
    view! {
        {move || {
            bench
                .get()
                .map(|b| {
                    let href = run_href(b.run_id);
                    view! {
                        <div class="gpu-k">"Benchmark"</div>
                        <div class="gpu-row bn-gpu-row">
                            <span class="chip live">
                                <span class="dot"></span>
                                {format!("run {} · {}", b.run_id, b.model_id)}
                            </span>
                        </div>
                        <div class="gpu-row dim">
                            "Holds the whole card until it ends: local models answer from their hold fallback or are refused (503 gpu_benchmark). "
                            <span on:click=move |_| open.set(false)>
                                <leptos_router::components::A href=href>"Open the run"</leptos_router::components::A>
                            </span>
                        </div>
                    }
                })
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_threshold_is_a_percentage_or_nothing() {
        assert_eq!(parse_threshold("5"), Some(5.0));
        assert_eq!(parse_threshold(" 2.5 % "), Some(2.5));
        assert_eq!(parse_threshold("0"), Some(0.0));
        assert_eq!(parse_threshold("-1"), None);
        assert_eq!(parse_threshold("abc"), None);
        assert_eq!(parse_threshold(""), None);
    }

    #[test]
    fn compare_ids_keep_their_order_and_drop_repeats_and_junk() {
        assert_eq!(parse_ids("9,8,x,9, 12"), [9, 8, 12]);
        assert!(parse_ids("").is_empty());
        assert_eq!(compare_href(&[8, 9]), "/benchmarks?compare=8,9");
    }

    #[test]
    fn numbers_read_like_a_person_writes_them() {
        assert_eq!(fmt_rate(12_202.1), "12\u{202F}202");
        assert_eq!(fmt_rate(69.017), "69.0");
        assert_eq!(fmt_rate(1.6635), "1.66");
        assert_eq!(fmt_rate(2.0), "2");
        assert_eq!(fmt_tokj(4.0), "4");
        assert_eq!(fmt_tokj(1.6635), "1.66");
        assert_eq!(fmt_tokj(42.51), "42.5");
        assert_eq!(fmt_ms(41.2), "41 ms");
        assert_eq!(fmt_ms(3090.0), "3.09 s");
        assert_eq!(fmt_ms(20_200.0), "20.2 s");
        assert_eq!(fmt_ms(150_000.0), "2m 30s");
        assert_eq!(tokens_short(512), "512");
        assert_eq!(tokens_short(2048), "2k");
        assert_eq!(tokens_short(262_142), "256k");
        assert_eq!(tokens_short(127_998), "125k");
        assert_eq!(fmt_delta(3.21), "+3.2 %");
        assert_eq!(fmt_delta(-15.25), "\u{2212}15.2 %");
        assert_eq!(fmt_delta(-0.007), "±0.0 %");
        assert_eq!(fmt_rate(0.0), "0");
        assert_eq!(fmt_ms_axis(100.0), "100 ms");
        assert_eq!(fmt_ms_axis(2000.0), "2 s");
        assert_eq!(fmt_ms_axis(1500.0), "1.5 s");
    }

    #[test]
    fn a_build_is_its_slug_and_version_or_commit() {
        assert_eq!(
            build_words(
                "localhost/lmgw-llama-server:official-master",
                Some("official-master"),
                Some("b11226"),
                Some("0c6a6a7ce5")
            ),
            "official-master · b11226"
        );
        assert_eq!(
            build_words(
                "localhost/x:ik-main",
                Some("ik-main"),
                None,
                Some("0c6a6a7ce5")
            ),
            "ik-main · 0c6a6a7"
        );
        assert_eq!(
            build_words("ghcr.io/ggml-org/llama.cpp:server-cuda", None, None, None),
            "server-cuda"
        );
    }

    #[test]
    fn a_done_run_that_missed_a_phase_is_partial() {
        let (c, w, _) = status_look(BenchStatus::Done, false, false, None, None);
        assert_eq!((c, w.as_str()), ("chip warn", "partial"));
        let (c, w, t) = status_look(BenchStatus::Done, true, true, None, None);
        assert_eq!((c, w.as_str()), ("chip warn", "done, throttled"));
        assert!(t.contains("throttled numbers"));
        let (_, w, _) = status_look(BenchStatus::Aborted, false, false, Some("hold"), None);
        assert_eq!(w, "aborted: hold");
    }

    #[test]
    fn the_entry_points_link_to_the_prefilled_modal() {
        assert_eq!(
            new_href(Some("qwen3.5-0.8b"), None),
            "/benchmarks?new=1&model=qwen3.5-0.8b"
        );
        assert_eq!(new_href(None, None), "/benchmarks?new=1");
        assert_eq!(
            new_href(None, Some("localhost/lmgw-llama-server:ik-main")),
            "/benchmarks?new=1&image=localhost/lmgw-llama-server:ik-main"
        );
        assert_eq!(query_value("a b&c=d#e"), "a%20b%26c%3Dd%23e");
    }
}
