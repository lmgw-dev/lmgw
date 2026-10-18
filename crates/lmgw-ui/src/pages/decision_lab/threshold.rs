//! A question's threshold slider (labs-store design §3.4, §3.5 R14): it
//! moves over the full curve the metrics return, one stop per distinct
//! confidence (the certainty max(p, 1 − p) for `noul`), lowest first, and
//! reads "at 0.72: 81% covered, 96.4% correct (n = 812)": the share of the
//! answered items at or above the cut, and of the labelled ones among them
//! the share answered right. "Use as threshold" posts the cut to the run's
//! threshold route, which measures it from the stored runs and writes the
//! value and its measurement into the saved set the run ran. A cut with no
//! labelled answer at or above it measures nothing (the server's 409
//! `no_labels`): the button says so before it is pressed, and a refusal
//! that comes anyway (`no_labels`, `set_deleted`, `set_changed`) is said
//! under it. The saved set is read by the page: until it lands, or when its
//! read failed (said with Retry), the button says why it waits; a deleted
//! set (a 404) says so.
//!
//! The slider starts on the set's cut when it was measured on this run, and
//! follows the set while it is read; once moved by hand it keeps its cut
//! across reads of the metrics (the bins, Refresh, the job's end), on the
//! nearest point of the new curve.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::{CurvePoint, QuestionMetrics, ThresholdWrite};
use lmgw_api_types::lab_store::LabPresetView;

use super::batch_view::{PresetRead, RunPage};
use super::sets::set_of;
use crate::api;
use crate::fmt::{grouped, pct};

/// A cut as the page writes it: at most three decimals, no trailing zeros
/// ("0.72", "0.815", "1").
pub(super) fn cut(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" {
        "0".into()
    } else {
        s.to_string()
    }
}

/// "81% covered, 96.4% correct (n = 812)" from (coverage, accuracy,
/// labelled); "not measured" without them.
pub(super) fn measured_text(m: Option<(f64, f64, u64)>) -> String {
    match m {
        Some((coverage, accuracy, n)) => format!(
            "{} covered, {:.1}% correct (n = {})",
            pct(coverage),
            accuracy * 100.0,
            grouped(n)
        ),
        None => "not measured".into(),
    }
}

/// The slider's line for one curve point.
pub(super) fn at_text(p: &CurvePoint) -> String {
    match p.accuracy {
        Some(a) => format!(
            "at {}: {}",
            cut(p.threshold),
            measured_text(Some((p.coverage, a, p.labelled)))
        ),
        None => format!(
            "at {}: {} covered, no labelled answer at or above it",
            cut(p.threshold),
            pct(p.coverage)
        ),
    }
}

/// Where the slider starts: on the set's own cut when it was measured on
/// this run (the first point at or above it), else at the lowest, which
/// covers every answer.
pub(super) fn start_at(curve: &[CurvePoint], measured_here: Option<f64>) -> usize {
    measured_here
        .and_then(|v| curve.iter().position(|p| p.threshold >= v - 1e-12))
        .unwrap_or(0)
}

/// The point of `curve` nearest the cut `v`: a cut chosen by hand, kept
/// across a read whose curve moved.
pub(super) fn snap(curve: &[CurvePoint], v: f64) -> usize {
    curve
        .iter()
        .enumerate()
        .min_by(|a, b| {
            (a.1.threshold - v)
                .abs()
                .total_cmp(&(b.1.threshold - v).abs())
        })
        .map_or(0, |(i, _)| i)
}

/// The cut the saved set holds for `qid` when it was measured on run
/// `batch`.
pub(super) fn measured_here(p: &PresetRead, qid: &str, batch: i64) -> Option<f64> {
    let t = set_of(p.view()?).thresholds.get(qid)?;
    t.measured
        .as_ref()
        .filter(|m| m.batch_id == batch)
        .map(|_| t.value)
}

/// What the saved set says of this question's threshold now.
pub(super) fn set_line(p: &PresetRead, qid: &str, batch: i64) -> String {
    let p = match p {
        PresetRead::Read(p) => p,
        PresetRead::Unsaved => {
            return "this run ran an unsaved set: its thresholds cannot be written".into()
        }
        PresetRead::Deleted => {
            return "the set this run ran is deleted: its thresholds cannot be written".into()
        }
        PresetRead::Reading => return "reading the set…".into(),
        PresetRead::Failed(e) => return format!("the set did not read: {e}"),
    };
    let t = set_of(p).thresholds.get(qid).cloned().unwrap_or_default();
    match t.measured {
        Some(m) => format!(
            "the set '{}' uses {} · {} on {}",
            p.name,
            cut(t.value),
            measured_text(Some((m.coverage, m.accuracy, m.labelled))),
            if m.batch_id == batch {
                "this run".to_string()
            } else {
                format!("set run #{}", m.batch_id)
            }
        ),
        None if t.value > 0.0 => {
            format!("the set '{}' uses {} · not measured", p.name, cut(t.value))
        }
        None => format!("the set '{}': not measured", p.name),
    }
}

#[component]
pub(super) fn ThresholdSlider(page: RunPage, m: QuestionMetrics) -> impl IntoView {
    let qid = m.id.clone();
    let noul = m.kind == "noul";
    let curve = StoredValue::new(m.curve.clone());
    let n = m.curve.len();
    if n == 0 {
        return view! {
            <p class="mini-note dim">"No answer yet: the curve is empty."</p>
        }
        .into_any();
    }
    let cuts = page.metrics.cuts;
    let chosen = {
        let qid = qid.clone();
        cuts.with_value(|c| c.get(&qid).copied())
    };
    let at = RwSignal::new(match chosen {
        Some(v) => snap(&m.curve, v),
        None => untrack(|| {
            page.preset
                .with(|p| start_at(&m.curve, measured_here(p, &qid, page.id)))
        }),
    });
    // Until moved by hand, the slider follows the set: a set read late, or
    // read again, puts it on the cut measured here.
    {
        let qid = qid.clone();
        Effect::new(move |_| {
            let here = page.preset.with(|p| measured_here(p, &qid, page.id));
            if cuts.with_value(|c| c.contains_key(&qid)) {
                return;
            }
            let i = curve.with_value(|c| start_at(c, here));
            if at.get_untracked() != i {
                at.set(i);
            }
        });
    }
    let err = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let point = Memo::new(move |_| {
        let i = at.get();
        curve.with_value(|c| c.get(i).cloned())
    });
    let qid_line = qid.clone();
    let line = move || page.preset.with(|p| set_line(p, &qid_line, page.id));
    let why_not = move || -> Option<String> {
        if let Some(w) = page.preset.with(PresetRead::why_not) {
            return Some(w);
        }
        match point.get() {
            Some(p) if p.accuracy.is_none() => {
                Some("no labelled answer at or above this cut: it would measure nothing".into())
            }
            None => Some("no point".into()),
            _ => None,
        }
    };
    let failed_read = move || page.preset.with(|p| matches!(p, PresetRead::Failed(_)));
    let retry_read = move |_| {
        if let Some(pid) = page
            .view
            .with_untracked(|v| v.as_ref().and_then(|v| v.preset_id))
        {
            page.read_preset(pid);
        }
    };
    let qid_use = qid.clone();
    let use_it = move |_| {
        let Some(p) = point.get_untracked() else {
            return;
        };
        let body = ThresholdWrite {
            question: qid_use.clone(),
            value: p.threshold,
        };
        let id = page.id;
        busy.set(true);
        err.set(None);
        page.scope.spawn(async move {
            let got = api::post::<LabPresetView, _>(
                format!("/decision-lab/api/batches/{id}/threshold"),
                &body,
            )
            .await;
            busy.set(false);
            match got {
                Ok(v) => {
                    page.toasts.ok(format!(
                        "'{}' threshold {} written into the set '{}'",
                        body.question,
                        cut(body.value),
                        v.name
                    ));
                    page.took_preset(v);
                }
                Err(api::Error::Api(e)) => err.set(Some(match e.code.as_str() {
                    "no_labels" => format!("Not written: {}", e.message),
                    "set_deleted" | "set_changed" => format!("Not written: {}", e.message),
                    _ => e.message,
                })),
                Err(e) => err.set(Some(e.to_string())),
            }
        });
    };
    let cut_on = if noul {
        "certainty max(p, 1−p)"
    } else {
        "confidence"
    };
    view! {
        <div class="dl-thr">
            <input
                class="dl-thr-range"
                type="range"
                min="0"
                max=(n - 1).to_string()
                step="1"
                aria-label=format!("{qid}: the threshold on the {cut_on}")
                prop:value=move || at.get().to_string()
                on:input={
                    let qid = qid.clone();
                    move |ev| {
                        if let Ok(i) = event_target_value(&ev).parse::<usize>() {
                            let i = i.min(n - 1);
                            at.set(i);
                            if let Some(t) = curve.with_value(|c| c.get(i).map(|p| p.threshold)) {
                                cuts.update_value(|c| {
                                    c.insert(qid.clone(), t);
                                });
                            }
                        }
                    }
                }
            />
            <div class="dl-thr-ends mini-note dim">
                <span>{format!("{} · all", cut(m.curve[0].threshold))}</span>
                <span>{format!("{} distinct {}", grouped(n as u64), if noul { "certainties" } else { "confidences" })}</span>
                <span>{cut(m.curve[n - 1].threshold)}</span>
            </div>
            {move || point.get().map(|p| {
                let cov = p.coverage;
                let acc = p.accuracy.unwrap_or(0.0);
                view! {
                    <p class="dl-thr-at">{at_text(&p)}</p>
                    <div class="dl-thr-meters">
                        <span class="lbl">"covered"</span>
                        <span class="dl-track"><i style=format!("width:{:.2}%", cov * 100.0)></i></span>
                        <span class="lbl">"correct"</span>
                        <span class="dl-track"><i style=format!("width:{:.2}%", acc * 100.0)></i></span>
                    </div>
                }
            })}
            <div class="dl-thr-act">
                <button
                    class="btn sm primary"
                    disabled=move || busy.get() || why_not().is_some()
                    title=move || why_not().unwrap_or_else(|| "Write this cut and its measurement into the saved set the run ran".to_string())
                    on:click=use_it
                >
                    "Use as threshold"
                </button>
                {move || why_not().map(|w| view! { <span class="mini-note dim">{w}</span> })}
                <Show when=failed_read>
                    <button class="btn ghost sm" on:click=retry_read>"Retry"</button>
                </Show>
            </div>
            {move || err.get().map(|e| view! { <p class="mini-note bad dl-thr-err">{e}</p> })}
            <p class="mini-note dim dl-thr-set">{line}</p>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::decision_lab::{Measured, QuestionSet, Threshold};
    use lmgw_api_types::lab_store::{LabKind, LabPresetBody};

    fn pt(threshold: f64, accuracy: Option<f64>) -> CurvePoint {
        CurvePoint {
            threshold,
            covered: 812,
            coverage: 0.81,
            labelled: 812,
            correct: 783,
            accuracy,
        }
    }

    #[test]
    fn the_line_reads_as_the_spec_writes_it() {
        assert_eq!(
            at_text(&pt(0.72, Some(0.964))),
            "at 0.72: 81% covered, 96.4% correct (n = 812)"
        );
        assert_eq!(
            at_text(&pt(0.9, None)),
            "at 0.9: 81% covered, no labelled answer at or above it"
        );
        assert_eq!(cut(0.8154), "0.815");
        assert_eq!(cut(1.0), "1");
        assert_eq!(cut(0.0), "0");
    }

    #[test]
    fn the_slider_starts_on_the_cut_measured_here() {
        let c = [pt(0.2, Some(0.5)), pt(0.5, Some(0.8)), pt(0.72, Some(0.9))];
        assert_eq!(start_at(&c, None), 0);
        assert_eq!(start_at(&c, Some(0.5)), 1);
        assert_eq!(start_at(&c, Some(0.6)), 2);
        assert_eq!(start_at(&c, Some(0.99)), 0, "past the curve: the lowest");
    }

    #[test]
    fn a_cut_chosen_by_hand_snaps_to_the_nearest_point_of_a_new_curve() {
        let c = [pt(0.2, Some(0.5)), pt(0.5, Some(0.8)), pt(0.72, Some(0.9))];
        assert_eq!(snap(&c, 0.5), 1, "still on the curve");
        assert_eq!(snap(&c, 0.66), 2);
        assert_eq!(snap(&c, 0.3), 0);
        assert_eq!(snap(&c, 0.95), 2);
        assert_eq!(snap(&[], 0.5), 0);
    }

    #[test]
    fn the_set_line_names_where_the_threshold_was_measured() {
        let thresholds = [(
            "team",
            Threshold {
                value: 0.72,
                measured: Some(Measured {
                    batch_id: 4,
                    coverage: 0.81,
                    accuracy: 0.964,
                    labelled: 812,
                }),
            },
        )]
        .into_iter()
        .collect();
        let set = QuestionSet {
            thresholds,
            ..Default::default()
        };
        let p = PresetRead::Read(LabPresetView {
            id: 1,
            lab: LabKind::Decision,
            name: "triage".into(),
            preset: LabPresetBody::QuestionSet(set),
            created_at: String::new(),
            updated_at: String::new(),
        });
        assert_eq!(measured_here(&p, "team", 4), Some(0.72));
        assert_eq!(
            measured_here(&p, "team", 5),
            None,
            "measured on another run"
        );
        assert_eq!(measured_here(&PresetRead::Reading, "team", 4), None);
        assert_eq!(
            set_line(&p, "team", 4),
            "the set 'triage' uses 0.72 · 81% covered, 96.4% correct (n = 812) on this run"
        );
        assert!(set_line(&p, "team", 5).ends_with("on set run #4"));
        assert_eq!(set_line(&p, "urgency", 4), "the set 'triage': not measured");
        assert!(set_line(&PresetRead::Unsaved, "team", 4).contains("unsaved"));
        assert!(set_line(&PresetRead::Deleted, "team", 4).contains("deleted"));
        assert!(set_line(&PresetRead::Failed("x".into()), "team", 4).contains("did not read"));
    }
}
