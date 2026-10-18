//! A set run's metrics (labs-store design §3.4, §3.5), read from
//! `/decision-lab/api/batches/{id}/metrics?bins=<n>`: per question the
//! counts, the accuracy (and a score's MAE), the confusion matrix, the
//! reliability diagram and the threshold slider. `bins` is a visible field,
//! 10 by default, with no maximum: the server answers only the bins that
//! hold an answer, and the diagram draws the empty ones from `bins`.
//!
//! The metrics are read when the page opens, when `bins` changes, when the
//! run's job ends and on Refresh; while the run runs, the line under the
//! head says how many runs have landed since they were read. A read draws
//! the question cards again; a threshold cut chosen by hand is kept per
//! question across reads (`cuts`).
//!
//! Without labels the run is bulk classification: the section says so, and
//! the items table and the export are its result.

use std::collections::HashMap;

use leptos::prelude::*;
use lmgw_api_types::decision_lab::{BatchMetrics, QuestionDraft, QuestionMetrics, DEFAULT_BINS};

use super::batch_view::RunPage;
use super::confusion::Confusion;
use super::reliability::Reliability;
use super::threshold::ThresholdSlider;
use crate::api;
use crate::fmt::grouped;
use crate::scope::{Latest, Scope};

/// The metrics as read. `Copy`.
#[derive(Clone, Copy)]
pub(super) struct Metrics {
    batch: i64,
    pub read: RwSignal<Option<Result<BatchMetrics, String>>>,
    /// The `bins` field as typed.
    pub bins: RwSignal<String>,
    /// How many runs the run had when they were read.
    pub at_done: RwSignal<Option<u64>>,
    pub loading: RwSignal<bool>,
    /// Per question id, the cut its slider was moved to by hand: kept
    /// across reads, on the nearest point of the new curve.
    pub cuts: StoredValue<HashMap<String, f64>>,
    reads: Latest,
    scope: Scope,
}

/// The `bins` field's value: a whole number, at least 1.
pub(super) fn bins_of(text: &str) -> Result<u32, String> {
    match text.trim().parse::<u32>() {
        Ok(0) => Err("bins: at least 1".into()),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("bins: '{}' is no whole number", text.trim())),
    }
}

impl Metrics {
    pub fn new(batch: i64, scope: Scope) -> Self {
        Self {
            batch,
            read: RwSignal::new(None),
            bins: RwSignal::new(DEFAULT_BINS.to_string()),
            at_done: RwSignal::new(None),
            loading: RwSignal::new(false),
            cuts: StoredValue::new(HashMap::new()),
            reads: Latest::new(),
            scope,
        }
    }

    /// Read them again with the bins as typed; a field that is no number is
    /// said beside it, and the last metrics stay.
    pub fn reload(self) {
        let Ok(bins) = bins_of(&self.bins.get_untracked()) else {
            return;
        };
        let ticket = self.reads.next();
        let href = format!(
            "/decision-lab/api/batches/{}/metrics?bins={bins}",
            self.batch
        );
        self.loading.set(true);
        self.scope.spawn(async move {
            let got = api::get::<BatchMetrics>(href).await;
            if !ticket.is_some_and(|t| self.reads.is(t)) {
                return;
            }
            self.loading.set(false);
            self.read.set(Some(got.map_err(|e| e.to_string())));
        });
    }
}

/// "accuracy 92.0% (46 of 50) · MAE 0.31 levels".
pub(super) fn accuracy_line(m: &QuestionMetrics) -> String {
    let mut s = match m.accuracy {
        Some(a) => format!(
            "accuracy {:.1}% ({} of {} scored)",
            a * 100.0,
            grouped((a * m.scored as f64).round() as u64),
            grouped(m.scored)
        ),
        None => "accuracy: nothing scored (no item is both answered and labelled)".into(),
    };
    if let Some(mae) = m.mae {
        s.push_str(&format!(" · MAE {mae:.2} levels"));
    }
    s
}

/// "50 items · 48 answered · 1 failed · 1 unreadable · 50 labelled".
pub(super) fn counts_line(m: &QuestionMetrics) -> String {
    format!(
        "{} items · {} answered · {} failed · {} unreadable · {} labelled",
        grouped(m.items),
        grouped(m.answered),
        grouped(m.failed),
        grouped(m.unreadable),
        grouped(m.labelled)
    )
}

#[component]
pub(super) fn MetricsSection(page: RunPage) -> impl IntoView {
    let m = page.metrics;
    let labelled = Memo::new(move |_| page.view.with(|v| v.as_ref().map(|v| v.labelled)));
    // Read once the run is known to have labels.
    Effect::new(move |done: Option<bool>| {
        let has = labelled.get().is_some_and(|n| n > 0);
        if has && done != Some(true) {
            m.reload();
        }
        has || done == Some(true)
    });
    // What the metrics were read at, for the "since" line: the progress
    // when they landed, read untracked so a progress frame does not move it.
    Effect::new(move |_| {
        if m.read.with(|r| matches!(r, Some(Ok(_)))) {
            m.at_done.set(untrack(|| page.progress()).map(|p| p.done));
        }
    });
    let bins_err = Memo::new(move |_| bins_of(&m.bins.get()).err());
    let since = move || {
        let now = page.progress()?.done;
        let at = m.at_done.get()?;
        (now > at).then(|| format!("{} runs landed since these were read", grouped(now - at)))
    };
    let questions = Memo::new(move |_| {
        page.view
            .with(|v| v.as_ref().map(|v| v.set.questions.clone()))
            .unwrap_or_default()
    });

    move || {
        match labelled.get() {
        None => ().into_any(),
        Some(0) => view! {
            <div class="card dl-metrics-none">
                <h3 class="lab-h3">"Metrics"</h3>
                <p class="mini-note dim">
                    "No item carries a label: this run is bulk classification. Its answers are the items below and the export; map a label column per question in a new set run to measure accuracy and a threshold."
                </p>
            </div>
        }
        .into_any(),
        Some(_) => view! {
            <div class="card dl-metrics">
                <div class="dl-card-head">
                    <h3 class="lab-h3">"Metrics"</h3>
                    <span class="spacer"></span>
                    <label class="dl-bins" title="Equal-width bins of the predicted probability in the reliability diagram; any number, at least 1">
                        <span class="lbl">"bins"</span>
                        <input
                            class="input w-num"
                            type="number"
                            min="1"
                            step="1"
                            prop:value=move || m.bins.get()
                            on:input=move |ev| m.bins.set(event_target_value(&ev))
                            on:change=move |_| m.reload()
                        />
                    </label>
                    <button
                        class="btn ghost sm"
                        disabled=move || m.loading.get() || bins_err.get().is_some()
                        title="Read the metrics again from the runs kept now"
                        on:click=move |_| m.reload()
                    >
                        {move || if m.loading.get() { "reading…" } else { "Refresh" }}
                    </button>
                </div>
                {move || bins_err.get().map(|e| view! { <p class="mini-note bad">{e}</p> })}
                {move || since().map(|s| view! { <p class="mini-note dl-warn-line">{s}</p> })}
                {move || match m.read.get() {
                    None => view! { <div class="dim lh-loading">"Reading the metrics…"</div> }.into_any(),
                    Some(Err(e)) => view! {
                        <div class="notice err">{format!("⚠ The metrics did not load: {e}")}</div>
                    }
                    .into_any(),
                    Some(Ok(bm)) => {
                        let qs = questions.get();
                        bm.questions
                            .into_iter()
                            .map(|qm| {
                                let q = qs.iter().find(|q| q.id == qm.id).cloned();
                                view! { <QuestionCard page=page m=qm q=q bins=bm.bins/> }
                            })
                            .collect_view()
                            .into_any()
                    }
                }}
            </div>
        }
        .into_any(),
    }
    }
}

#[component]
fn QuestionCard(
    page: RunPage,
    m: QuestionMetrics,
    q: Option<QuestionDraft>,
    bins: u32,
) -> impl IntoView {
    let id = m.id.clone();
    let kind = m.kind.clone();
    view! {
        <section class="dl-qmetrics">
            <div class="dl-qm-head">
                <b class="mono">{id.clone()}</b>
                <span class="chip off">{kind}</span>
                <span class="dl-qm-acc">{accuracy_line(&m)}</span>
            </div>
            <p class="mini-note dim dl-qm-counts">{counts_line(&m)}</p>
            <div class="dl-qm-grid">
                <div class="dl-qm-part">
                    <h4 class="dl-qm-h">"Confusion"</h4>
                    <Confusion m=m.clone() q=q.clone()/>
                </div>
                <div class="dl-qm-part">
                    <h4 class="dl-qm-h">"Reliability"</h4>
                    <Reliability m=m.clone() bins=bins/>
                </div>
                <div class="dl-qm-part">
                    <h4 class="dl-qm-h">"Threshold"</h4>
                    <ThresholdSlider page=page m=m.clone()/>
                </div>
            </div>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qm(accuracy: Option<f64>, scored: u64, mae: Option<f64>) -> QuestionMetrics {
        QuestionMetrics {
            id: "team".into(),
            kind: "choice".into(),
            classes: vec![],
            items: 50,
            answered: 48,
            failed: 1,
            unreadable: 1,
            labelled: 50,
            scored,
            accuracy,
            mae,
            confusion: vec![],
            reliability: vec![],
            curve: vec![],
        }
    }

    #[test]
    fn the_lines_say_accuracy_and_the_counts() {
        assert_eq!(
            accuracy_line(&qm(Some(0.92), 50, None)),
            "accuracy 92.0% (46 of 50 scored)"
        );
        assert_eq!(
            accuracy_line(&qm(Some(0.5), 4, Some(0.314))),
            "accuracy 50.0% (2 of 4 scored) · MAE 0.31 levels"
        );
        assert!(accuracy_line(&qm(None, 0, None)).contains("nothing scored"));
        assert_eq!(
            counts_line(&qm(None, 0, None)),
            "50 items · 48 answered · 1 failed · 1 unreadable · 50 labelled"
        );
    }

    #[test]
    fn bins_are_a_whole_number_of_at_least_one() {
        assert_eq!(bins_of(" 12 "), Ok(12));
        assert!(bins_of("0").is_err());
        assert!(bins_of("ten").unwrap_err().contains("'ten'"));
        assert_eq!(bins_of("100000"), Ok(100_000), "no maximum");
    }
}
