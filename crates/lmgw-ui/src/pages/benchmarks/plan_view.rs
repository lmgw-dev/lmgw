//! What a start would do (`bench_plan`, benchmark design §8.1), in the New
//! benchmark modal: the provisional points of every selected phase, the
//! probes that would run and why the others would not, the command line,
//! warnings, and — above all of it — why it cannot start, when it cannot.
//! Then the confirmation (§3.2, "confirm, then do"): every container the
//! start stops, and what the run means for local models while it lasts.

use leptos::prelude::*;
use lmgw_api_types::bench::Phase;
use lmgw_api_types::bench_ops::{BenchPlan, BlockedReason};

use super::probes::{probe_about, probe_label};
use super::tokens_short;
use crate::fmt::grouped;
use crate::widgets::CopyBtn;

/// "512 · 2k · 8k · 32k · 128k · 256k".
fn lengths(v: &[u64]) -> String {
    v.iter()
        .map(|x| tokens_short(*x))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The provisional points of the selected phases, one line each.
fn point_lines(plan: &BenchPlan, phases: &[Phase]) -> Vec<(&'static str, String)> {
    let p = &plan.points;
    let mut out = Vec::new();
    let has = |ph: Phase| phases.contains(&ph);
    if has(Phase::Prefill) {
        out.push((
            "prefill",
            if p.prefill.is_empty() {
                "no point fits the context".to_string()
            } else {
                format!("{} tokens of prompt", lengths(&p.prefill))
            },
        ));
    }
    if has(Phase::Decode) {
        out.push((
            "decode",
            if p.decode.is_empty() {
                "no depth fits the context".to_string()
            } else {
                format!(
                    "{} tokens deep, {} generated each",
                    lengths(&p.decode),
                    p.generate_tokens
                )
            },
        ));
    }
    if has(Phase::Concurrent) {
        let n: Vec<String> = p.concurrent.iter().map(u32::to_string).collect();
        out.push((
            "concurrent",
            if n.is_empty() {
                "none".to_string()
            } else {
                format!("{} streams", n.join(" · "))
            },
        ));
    }
    if has(Phase::Mixed) {
        out.push((
            "mixed",
            match &p.mixed {
                Some(m) => format!(
                    "{} decoding streams, then a prompt of {} tokens arrives",
                    m.streams,
                    tokens_short(m.inject_tokens)
                ),
                None => "skipped: it needs at least two slots".to_string(),
            },
        ));
    }
    out
}

#[component]
pub fn PlanPreview(plan: BenchPlan, phases: Vec<Phase>, stale: Signal<bool>) -> impl IntoView {
    let lines = point_lines(&plan, &phases);
    let mut p = plan.points.clone();
    // A note about a phase that is not selected, or one the points line
    // above already says (mixed's single slot), is not repeated.
    p.notes.retain(|n| {
        let head = n.split(':').next().unwrap_or_default().trim();
        match Phase::parse(head) {
            Some(Phase::Mixed) => plan.points.mixed.is_some() && phases.contains(&Phase::Mixed),
            Some(ph) => phases.contains(&ph),
            None => true,
        }
    });
    let probes_on = phases.contains(&Phase::Probes);
    let reps = plan.params.repetitions;
    let probes = plan
        .probes
        .iter()
        .map(|pp| {
            let (class, title) = match &pp.skip {
                None => ("chip ok", probe_about(pp.probe).to_string()),
                Some(why) => ("chip off", format!("skipped: {why}")),
            };
            view! {
                <span class=format!("{class} bn-probe-chip") title=title>
                    {probe_label(pp.probe)}
                    {pp.skip.is_some().then_some(" — skipped")}
                </span>
            }
        })
        .collect_view();
    let skips: Vec<String> = plan
        .probes
        .iter()
        .filter_map(|pp| {
            pp.skip
                .as_ref()
                .map(|w| format!("{}: {w}", probe_label(pp.probe)))
        })
        .collect();
    let slots = format!(
        "{} {} × {} tokens per slot",
        p.n_slots,
        if p.n_slots == 1 { "slot" } else { "slots" },
        grouped(p.per_slot_ctx)
    );
    view! {
        <div class="bn-plan" class:stale=move || stale.get()>
            {plan
                .blocked
                .clone()
                .map(|b| {
                    let (class, head) = blocked_look(b.reason);
                    view! {
                        <div class=format!("{class} bn-blocked")>
                            <b>{head}</b>
                            {b.message}
                        </div>
                    }
                })}
            {(!plan.warnings.is_empty())
                .then(|| {
                    view! {
                        <div class="notice warn">
                            {plan.warnings.iter().map(|w| view! { <span class="detail">{w.clone()}</span> }).collect_view()}
                        </div>
                    }
                })}
            <div class="bn-plan-sec">
                <h3>"Points"</h3>
                <p class="dim bn-plan-note">
                    {format!(
                        "Provisional, from the row's numbers ({slots}); the run reads the real slot count and context from the server and derives them again. {} {} each.",
                        crate::fmt::count_of(reps as usize, "repetitions"),
                        if reps == 1 { "of every point" } else { "of every point, median, min and max kept" },
                    )}
                </p>
                <dl class="bn-kv">
                    <dt>"load"</dt>
                    <dd>"podman run → /health, and the VRAM it took (always)"</dd>
                    {lines.into_iter().map(|(k, v)| view! { <dt>{k}</dt><dd>{v}</dd> }).collect_view()}
                </dl>
                {(!p.notes.is_empty())
                    .then(|| view! { <p class="dim bn-plan-note">{p.notes.join(" · ")}</p> })}
                <p class="dim bn-plan-note">
                    {format!("{} progress steps. The top points come from the whole context, so a long-context row costs a long prefill; drop a phase to save it.", plan.total_steps)}
                </p>
            </div>
            <Show when=move || probes_on>
                <div class="bn-plan-sec">
                    <h3>"Probes"</h3>
                    <div class="bn-probe-chips">{probes.clone()}</div>
                    {(!skips.is_empty()).then(|| view! { <p class="dim bn-plan-note">{skips.join(" · ")}</p> })}
                </div>
            </Show>
            <div class="bn-plan-sec">
                <div class="cmd-head">
                    <h3>"Command line"</h3>
                    <span class="dim">"the port and run id are the run's own"</span>
                    <CopyBtn text=plan.command_line.clone() title="Copy the command line"/>
                </div>
                <pre class="preset cmd-lines bn-plan-cmd">{plan.command_line.clone()}</pre>
            </div>
        </div>
    }
}

/// The confirmation: what the start stops, and what the run means.
#[component]
pub fn Confirmation(plan: BenchPlan) -> impl IntoView {
    let rung = if plan.rungs > 1 {
        format!(" (rung {} of {})", plan.rung, plan.rungs - 1)
    } else {
        String::new()
    };
    let phases: Vec<&str> = plan
        .params
        .phases
        .iter()
        .map(|p| super::phase_label(*p))
        .collect();
    let head = format!(
        "Benchmark {}{rung} on {} — {} · {} · {} steps",
        plan.model_id,
        plan.settings.image,
        phases.join(", "),
        crate::fmt::count_of(plan.params.repetitions as usize, "repetitions"),
        plan.total_steps
    );
    let stops = plan.stops.clone();
    view! {
        <div class="bn-confirm">
            <p class="bn-confirm-head">{head}</p>
            {plan
                .blocked
                .clone()
                .map(|b| {
                    let (class, head) = blocked_look(b.reason);
                    view! { <div class=class><b>{head}</b>{b.message}</div> }
                })}
            <div class="bn-plan-sec">
                <h3>{format!("Stopped first ({})", stops.len())}</h3>
                {if stops.is_empty() {
                    view! { <p class="dim">"Nothing else is on the GPU."</p> }.into_any()
                } else {
                    view! {
                        <p class="dim bn-plan-note">
                            "Every lmgw container on the card is stopped, whatever its class. An idle one at once; a busy one once its requests finish — nothing is cut off mid-request."
                        </p>
                        <table class="data bk-sub bn-stops">
                            <tbody>
                                {stops
                                    .into_iter()
                                    .map(|s| {
                                        let busy = s.busy.then(|| {
                                            let n = if s.in_flight > 0 {
                                                format!(" ({} in flight)", s.in_flight)
                                            } else {
                                                String::new()
                                            };
                                            view! {
                                                <span class="chip warn">
                                                    <span class="dot"></span>
                                                    {format!("will finish its request first{n}")}
                                                </span>
                                            }
                                        });
                                        view! {
                                            <tr>
                                                <td><span class="type-badge">{s.class.clone()}</span></td>
                                                <td class="mono-sm">{s.model_id.clone()}</td>
                                                <td class="clip mono-sm dim" title=s.container_name.clone()>{s.container_name.clone()}</td>
                                                <td class="dim">{s.state.clone()}</td>
                                                <td>{busy}</td>
                                            </tr>
                                        }
                                    })
                                    .collect_view()}
                            </tbody>
                        </table>
                    }
                        .into_any()
                }}
            </div>
            <div class="notice warn">
                <b>"While it runs, the GPU is the benchmark's"</b>
                "Local models of every class are unavailable until the run ends or is canceled: a request for one is answered by its hold fallback where one is configured, and refused with 503 "
                <code>"gpu_benchmark"</code>
                " otherwise. The stopped models are not started again afterwards; each loads on its next request."
            </div>
        </div>
    }
}

/// `(class, head)` of a refusal: one that clears by itself or by the owner's
/// switch (the hold, a run going, boot still adopting containers) is a
/// warning — "not yet"; one the request itself must change is an error.
pub fn blocked_look(reason: BlockedReason) -> (&'static str, &'static str) {
    match reason {
        BlockedReason::Hold | BlockedReason::RunGoing | BlockedReason::Booting => {
            ("notice warn", "Cannot start yet")
        }
        BlockedReason::RowMissing
        | BlockedReason::RowDisabled
        | BlockedReason::NotChat
        | BlockedReason::RungOutOfRange
        | BlockedReason::ImageMissing
        | BlockedReason::NoWeights
        | BlockedReason::InvalidImage => ("notice err", "Cannot start"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::bench::{MixedPlan, PointPlan};

    #[test]
    fn only_the_selected_phases_are_listed() {
        let plan = BenchPlan {
            points: PointPlan {
                per_slot_ctx: 262_144,
                n_slots: 1,
                generate_tokens: 256,
                prefill: vec![512, 2048, 262_142],
                decode: vec![64, 1024],
                concurrent: vec![1],
                ..Default::default()
            },
            ..Default::default()
        };
        let l = point_lines(&plan, &[Phase::Load, Phase::Prefill, Phase::Mixed]);
        assert_eq!(l.len(), 2);
        assert_eq!(
            l[0],
            ("prefill", "512 · 2k · 256k tokens of prompt".to_string())
        );
        assert!(l[1].1.contains("two slots"));
        let mut two = plan.clone();
        two.points.mixed = Some(MixedPlan {
            streams: 3,
            inject_tokens: 8192,
            ..Default::default()
        });
        let l = point_lines(&two, &[Phase::Mixed]);
        assert_eq!(
            l[0].1,
            "3 decoding streams, then a prompt of 8k tokens arrives"
        );
    }

    /// Review finding 8: `booting` and `invalid_image` read sensibly — boot
    /// clears by itself (a warning), a bad image override is the request's
    /// to fix (an error).
    #[test]
    fn transient_refusals_are_warnings_and_the_rest_errors() {
        assert_eq!(
            blocked_look(BlockedReason::Booting),
            ("notice warn", "Cannot start yet")
        );
        assert_eq!(blocked_look(BlockedReason::Hold).0, "notice warn");
        assert_eq!(
            blocked_look(BlockedReason::InvalidImage),
            ("notice err", "Cannot start")
        );
        assert_eq!(blocked_look(BlockedReason::ImageMissing).0, "notice err");
    }
}
