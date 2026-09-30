//! Runs side by side (`?compare=<id>,<id>,…`, benchmark design §8.2): their
//! curves overlaid, one colour per run in the order they were picked, each
//! later run's delta table against the first, and their probes side by side.
//!
//! The verdicts come from `lmgw_api_types::bench_compare` — the rule the ops
//! judge a run against its previous comparable one with — at the page's
//! threshold. Runs that are not comparable (§6: other weights, settings,
//! suite or GPU — two quants, say) are overlaid all the same, under a banner
//! that says why their deltas are not a verdict on a build.

use leptos::prelude::*;
use leptos_router::components::A;
use lmgw_api_types::bench_compare::{compare, not_comparable};
use lmgw_api_types::bench_ops::BenchRun;

use super::delta::{tally, DeltaTable, ThresholdControl};
use super::phase_charts::{PhaseCharts, RunLine};
use super::probes::ProbesSide;
use super::{build_words, run_href, use_bn};
use crate::bench_api as api;
use crate::charts::slot_color;
use crate::fmt::hue_for;
use crate::pages::backends::local_ts;
use crate::scope::Scope;
use crate::widgets::{PageFrame, Section};

#[component]
pub fn CompareView(ids: Vec<i64>) -> impl IntoView {
    let bn = use_bn();
    let scope = Scope::new();
    let n = ids.len();
    let ids = StoredValue::new(ids);
    // One slot per id, in the order given.
    let slots = RwSignal::new(vec![None::<Result<BenchRun, String>>; n]);
    let load = move || {
        for (i, id) in ids.get_value().into_iter().enumerate() {
            let threshold = bn.threshold.get_untracked();
            scope.spawn(async move {
                let res = api::run(id, threshold, false)
                    .await
                    .map(|d| d.run)
                    .map_err(|e| format!("run {id}: {e}"));
                slots.update(|s| {
                    if let Some(slot) = s.get_mut(i) {
                        *slot = Some(res);
                    }
                });
            });
        }
    };
    load();
    Effect::new(move |prev: Option<u64>| {
        let now = bn.rev.get();
        if prev.is_some_and(|p| p != now) {
            load();
        }
        now
    });

    let runs = Memo::new(move |_| {
        slots.with(|s| {
            s.iter()
                .filter_map(|r| r.as_ref().and_then(|r| r.as_ref().ok()).cloned())
                .collect::<Vec<_>>()
        })
    });
    let errors = Memo::new(move |_| {
        slots.with(|s| {
            s.iter()
                .filter_map(|r| r.as_ref().and_then(|r| r.as_ref().err()).cloned())
                .collect::<Vec<_>>()
        })
    });
    let loading = move || slots.with(|s| s.iter().any(Option::is_none));
    let lines = Memo::new(move |_| {
        runs.with(|rs| {
            rs.iter()
                .enumerate()
                .map(|(i, r)| RunLine {
                    run: r.clone(),
                    // Short in the charts' legends: the legend card above
                    // says what each run is.
                    label: format!("run {}", r.id),
                    color: slot_color(Some(i as u8)).to_string(),
                })
                .collect::<Vec<_>>()
        })
    });
    // The ids that could not be read, in the order asked for.
    let failed = Memo::new(move |_| {
        slots.with(|s| {
            ids.get_value()
                .into_iter()
                .zip(s.iter())
                .filter(|(_, r)| matches!(r, Some(Err(_))))
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        })
    });
    // Names the base actually used — the first run that loaded — and the
    // runs that did not, rather than the base asked for (review finding 7).
    let sub = move || {
        let loaded =
            (!loading()).then(|| runs.with(|rs| rs.iter().map(|r| r.id).collect::<Vec<_>>()));
        compare_sub(&ids.get_value(), loaded.as_deref(), &failed.get())
    };
    view! {
        <PageFrame
            title="Compare runs"
            sub=Signal::derive(sub)
            class="bench"
            actions=|| view! { <A href="/benchmarks" attr:class="btn ghost">"All runs"</A> }
        >
            {move || {
                errors
                    .get()
                    .into_iter()
                    .map(|e| view! { <div class="notice err">{e}</div> })
                    .collect_view()
            }}
            <Show when=move || !loading() fallback=|| view! { <div class="empty">"Loading the runs…"</div> }>
                <Legend lines=lines/>
                <Comparability runs=runs/>
                <Section title="Changes against the base" persist="bench.compare.delta">
                    <Deltas runs=runs/>
                </Section>
                <Section title="Phases" persist="bench.compare.charts">
                    <PhaseCharts lines=lines/>
                </Section>
                <Section title="Probes" persist="bench.compare.probes">
                    {move || {
                        let v = lines
                            .get()
                            .into_iter()
                            .map(|l| (format!("run {}", l.run.id), l.color, l.run))
                            .collect::<Vec<_>>();
                        view! { <div class="card pad0"><ProbesSide runs=v/></div> }
                    }}
                </Section>
            </Show>
        </PageFrame>
    }
}

/// The Compare view's subtitle: the later runs against the base. While the
/// runs load it names the ones asked for; once they have, the ones that
/// loaded — the first of those is the base the deltas are judged against —
/// and the ones that could not be read, the requested base first.
pub fn compare_sub(requested: &[i64], loaded: Option<&[i64]>, failed: &[i64]) -> String {
    let names = |ids: &[i64]| ids.iter().map(|i| format!("run {i}")).collect::<Vec<_>>();
    let shown = names(loaded.unwrap_or(requested));
    let mut out = match shown.split_first() {
        Some((first, rest)) if !rest.is_empty() => {
            format!("{} against {first}, the base", rest.join(", "))
        }
        Some((first, _)) => format!("{first} alone — pick at least two runs on the list"),
        None if loaded.is_some() => "no run could be read".to_string(),
        None => String::new(),
    };
    if loaded.is_some() && !failed.is_empty() {
        let base = requested.first().copied().filter(|b| failed.contains(b));
        out.push_str(&match base {
            Some(b) if !shown.is_empty() => {
                let others: Vec<i64> = failed.iter().copied().filter(|i| *i != b).collect();
                format!(
                    " (the requested base, run {b}, could not be read{})",
                    if others.is_empty() {
                        String::new()
                    } else {
                        format!("; nor could {}", names(&others).join(", "))
                    }
                )
            }
            _ => format!(" ({} could not be read)", names(failed).join(", ")),
        });
    }
    out
}

/// Which colour is which run, and the way to each one's detail.
#[component]
fn Legend(lines: Memo<Vec<RunLine>>) -> impl IntoView {
    view! {
        <div class="card bn-legend">
            {move || {
                lines
                    .get()
                    .into_iter()
                    .enumerate()
                    .map(|(i, l)| {
                        let r = &l.run;
                        let hue = hue_for(&r.model.model_id);
                        let when = local_ts(&r.created_at);
                        view! {
                            <div class="bn-legend-row">
                                <i class="slot-swatch bn-legend-sw" style=format!("background:{}", l.color)></i>
                                <A href=run_href(r.id) attr:class="row-link mono-sm">{format!("run {}", r.id)}</A>
                                <span class="model-chip" style=format!("--hue:{hue}")>
                                    <i></i>
                                    {r.model.model_id.clone()}
                                </span>
                                <span class="mono-sm dim">{r.model.quant.clone().unwrap_or_default()}</span>
                                <span class="mono-sm" title=r.image_ref.clone()>
                                    {build_words(
                                        &r.image_ref,
                                        r.build.engine_slug.as_deref(),
                                        r.build.version.as_deref(),
                                        r.build.commit.as_deref(),
                                    )}
                                </span>
                                <span class="dim">{when}</span>
                                {(i == 0).then(|| view! { <span class="type-badge">"base"</span> })}
                            </div>
                        }
                    })
                    .collect_view()
            }}
        </div>
    }
}

/// Why runs are not comparable to the base (§6), prominently: their deltas
/// then say how two different things differ, not whether a build regressed.
#[component]
fn Comparability(runs: Memo<Vec<BenchRun>>) -> impl IntoView {
    view! {
        {move || {
            runs.with(|rs| {
                let Some((base, rest)) = rs.split_first() else {
                    return ().into_any();
                };
                let lines: Vec<(i64, Vec<String>)> = rest
                    .iter()
                    .map(|r| (r.id, not_comparable(base, r)))
                    .filter(|(_, why)| !why.is_empty())
                    .collect();
                if lines.is_empty() {
                    return ().into_any();
                }
                let base_id = base.id;
                view! {
                    <div class="notice warn bn-notcmp">
                        <b>"Not comparable — overlaid all the same"</b>
                        {lines
                            .into_iter()
                            .map(|(id, why)| {
                                view! {
                                    <span class="detail">
                                        {format!("run {id} against run {base_id}: {}", why.join("; "))}
                                    </span>
                                }
                            })
                            .collect_view()}
                        <span class="detail dim">
                            "Comparable runs share the weights, the settings, the suite version and the GPU; only the build may differ. Two quants or two settings are compared here as they are, and their verdicts describe the difference, not a regression."
                        </span>
                    </div>
                }
                .into_any()
            })
        }}
    }
}

/// Every later run against the base, at the page's threshold.
#[component]
fn Deltas(runs: Memo<Vec<BenchRun>>) -> impl IntoView {
    let bn = use_bn();
    view! {
        <div class="bn-deltas">
            <div class="row bn-deltas-bar">
                <span class="dim">"Each later run against the first one picked; lower is better for TTFT, load, VRAM and the stall."</span>
                <span class="spacer"></span>
                <ThresholdControl/>
            </div>
            {move || {
                let t = bn.threshold.get();
                runs.with(|rs| {
                    let Some((base, rest)) = rs.split_first() else {
                        return ().into_any();
                    };
                    if rest.is_empty() {
                        return view! { <div class="empty">"One run: nothing to judge it against."</div> }.into_any();
                    }
                    rest.iter()
                        .map(|r| {
                            let c = compare(r, base, t);
                            let head = format!("run {} against run {}: {}", r.id, base.id, tally(&c));
                            view! {
                                <div class="card edit-section bn-cmp">
                                    <div class="cmd-head">
                                        <span class="bn-cmp-head">{head}</span>
                                    </div>
                                    <DeltaTable cmp=c base=format!("run {}", base.id) this=format!("run {}", r.id)/>
                                </div>
                            }
                        })
                        .collect_view()
                        .into_any()
                })
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::compare_sub;

    /// Review finding 7: `?compare=999,5,6` with run 999 unreadable used to
    /// say "run 5, run 6 against run 999, the base" while judging against
    /// run 5.
    #[test]
    fn the_subtitle_names_the_base_actually_used() {
        let asked = [999, 5, 6];
        assert_eq!(
            compare_sub(&asked, None, &[]),
            "run 5, run 6 against run 999, the base",
            "while loading: what was asked for"
        );
        assert_eq!(
            compare_sub(&asked, Some(&[5, 6]), &[999]),
            "run 6 against run 5, the base (the requested base, run 999, could not be read)"
        );
        assert_eq!(
            compare_sub(&[5, 999, 6], Some(&[5, 6]), &[999]),
            "run 6 against run 5, the base (run 999 could not be read)"
        );
        assert_eq!(
            compare_sub(&[5, 6], Some(&[5, 6]), &[]),
            "run 6 against run 5, the base"
        );
        assert_eq!(
            compare_sub(&[999, 5], Some(&[5]), &[999]),
            "run 5 alone — pick at least two runs on the list (the requested base, run 999, could not be read)"
        );
        assert_eq!(
            compare_sub(&[998, 5, 999, 6], Some(&[5, 6]), &[998, 999]),
            "run 6 against run 5, the base (the requested base, run 998, could not be read; nor could run 999)"
        );
        assert_eq!(
            compare_sub(&[998, 999], Some(&[]), &[998, 999]),
            "no run could be read (run 998, run 999 could not be read)"
        );
    }
}
