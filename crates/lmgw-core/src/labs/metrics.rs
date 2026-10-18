//! A set run's metrics (labs-store design §3.4) as pure functions over the
//! items' outcomes: what each answer predicts, accuracy against the labels
//! (and the mean absolute error of a `score`), the confusion matrix, the
//! reliability bins and the full threshold curve. The routes read the stored
//! runs into [`ItemOutcome`]s; nothing here touches the store.
//!
//! Reliability is sparse: only the bins that hold an answer are built, so
//! any number of bins costs what the answers do. An answer goes in the bin
//! whose reported edges hold it, `index / bins` to `(index + 1) / bins` as
//! `f64`, not in `floor(p × bins)`, which a rounding puts one bin low.
//!
//! Labels are spelled as the store keeps them (`labs::ingest`): an option's
//! name, a level's index from `"0"`, `yes` or `no`. What one answer predicts
//! ([`predict`], [`outcome`]) lives in `lmgw-api-types`, beside `read_label`,
//! so the page's items table reads a run exactly as these metrics do.

use std::collections::BTreeMap;

use lmgw_api_types::decision_lab::{
    CurvePoint, QuestionDraft, QuestionKind, QuestionMetrics, ReliabilityBin,
};

/// The one rule of what an answer predicts and how a run reads, shared with
/// the page (`lmgw-api-types`): this module, the `readable` judgement of a
/// set run's item and the CSV export all call it.
pub use lmgw_api_types::decision_lab::metrics::{
    certainty, classes, correct, is_2xx, outcome, predict, read_response, Outcome, Prediction,
};

/// One item: its label for the question and its outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemOutcome {
    pub expected: Option<String>,
    pub outcome: Outcome,
}

fn ratio(n: u64, of: u64) -> Option<f64> {
    (of > 0).then(|| n as f64 / of as f64)
}

/// The answered items as `(confidence, label, correct)`.
fn answered(items: &[ItemOutcome]) -> Vec<(f64, Option<&str>, bool)> {
    items
        .iter()
        .filter_map(|i| match &i.outcome {
            Outcome::Answered(p) => Some((
                p.confidence,
                i.expected.as_deref(),
                i.expected.as_deref() == Some(p.class.as_str()),
            )),
            _ => None,
        })
        .collect()
}

/// What a cut at `threshold` keeps of `items`.
pub fn at_threshold(items: &[ItemOutcome], threshold: f64) -> CurvePoint {
    let all = answered(items);
    let kept: Vec<_> = all.iter().filter(|(c, _, _)| *c >= threshold).collect();
    let labelled = kept.iter().filter(|(_, l, _)| l.is_some()).count() as u64;
    let correct = kept.iter().filter(|(_, l, ok)| l.is_some() && *ok).count() as u64;
    CurvePoint {
        threshold,
        covered: kept.len() as u64,
        coverage: ratio(kept.len() as u64, all.len() as u64).unwrap_or(0.0),
        labelled,
        correct,
        accuracy: ratio(correct, labelled),
    }
}

/// One point per distinct confidence among the answered items, lowest
/// first: each with what a cut there keeps.
pub fn curve(items: &[ItemOutcome]) -> Vec<CurvePoint> {
    let mut all = answered(items);
    let n = all.len() as u64;
    // Highest first, so each point's counts are the running sums so far.
    all.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut points: Vec<CurvePoint> = Vec::new();
    let (mut covered, mut labelled, mut correct) = (0u64, 0u64, 0u64);
    let mut i = 0;
    while i < all.len() {
        let t = all[i].0;
        while i < all.len() && all[i].0 == t {
            covered += 1;
            if all[i].1.is_some() {
                labelled += 1;
                if all[i].2 {
                    correct += 1;
                }
            }
            i += 1;
        }
        points.push(CurvePoint {
            threshold: t,
            covered,
            coverage: ratio(covered, n).unwrap_or(0.0),
            labelled,
            correct,
            accuracy: ratio(correct, labelled),
        });
    }
    points.reverse();
    points
}

/// The bin of `bins` equal-width bins over 0 to 1 that holds `p` (clamped
/// to 0 to 1), by the edges the answer reports: `k / bins` ≤ p <
/// `(k + 1) / bins`, the last bin holding 1.
pub fn bin_of(p: f64, bins: u32) -> u32 {
    let b = bins.max(1);
    let p = if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) };
    let edge = |k: u32| f64::from(k) / f64::from(b);
    let mut k = ((p * f64::from(b)).floor() as u64).min(u64::from(b - 1)) as u32;
    while k > 0 && p < edge(k) {
        k -= 1;
    }
    while k + 1 < b && p >= edge(k + 1) {
        k += 1;
    }
    k
}

/// The reliability bins of the labelled answers that hold one, lowest
/// first, of `bins` equal-width bins (`bins` ≥ 1; the last bin includes 1).
/// A bin's `observed` is the rate of what was predicted: the winner or the
/// nearest level being the label (`choice`, `score`), the label being yes
/// (`noul`, binned by P(yes)), the standard reliability diagram.
pub fn reliability(q: &QuestionDraft, items: &[ItemOutcome], bins: u32) -> Vec<ReliabilityBin> {
    let b = bins.max(1);
    let noul = matches!(q.kind, QuestionKind::Noul { .. });
    // (count, sum of predictions, hits) per non-empty bin.
    let mut filled: BTreeMap<u32, (u64, f64, u64)> = BTreeMap::new();
    for i in items {
        let (Outcome::Answered(p), Some(label)) = (&i.outcome, i.expected.as_deref()) else {
            continue;
        };
        let prob = p.probability.clamp(0.0, 1.0);
        let hit = if noul {
            label == "yes"
        } else {
            p.class == label
        };
        let bin = filled.entry(bin_of(prob, b)).or_default();
        bin.0 += 1;
        bin.1 += prob;
        bin.2 += u64::from(hit);
    }
    filled
        .into_iter()
        .map(|(k, (count, sum, hits))| ReliabilityBin {
            index: k,
            lower: f64::from(k) / f64::from(b),
            upper: f64::from(k + 1) / f64::from(b),
            count,
            mean_prediction: sum / count as f64,
            observed: hits as f64 / count as f64,
        })
        .collect()
}

/// Question `q`'s metrics over `items`, with `bins` reliability bins.
pub fn question(q: &QuestionDraft, items: &[ItemOutcome], bins: u32) -> QuestionMetrics {
    let classes = classes(q);
    let at = |c: &str| classes.iter().position(|k| k == c);
    let mut confusion = vec![vec![0u64; classes.len()]; classes.len()];
    let (mut answered, mut failed, mut unreadable) = (0, 0, 0);
    let (mut labelled, mut scored, mut hits) = (0, 0, 0);
    let mut abs_err = 0f64;
    for i in items {
        if i.expected.is_some() {
            labelled += 1;
        }
        match &i.outcome {
            Outcome::Pending => {}
            Outcome::Failed => failed += 1,
            Outcome::Unreadable => unreadable += 1,
            Outcome::Answered(p) => {
                answered += 1;
                let Some(label) = i.expected.as_deref() else {
                    continue;
                };
                scored += 1;
                if p.class == label {
                    hits += 1;
                }
                if let (Some(row), Some(col)) = (at(label), at(&p.class)) {
                    confusion[row][col] += 1;
                }
                if let (Some(s), Ok(l)) = (p.score, label.parse::<f64>()) {
                    abs_err += (s - l).abs();
                }
            }
        }
    }
    let is_score = matches!(q.kind, QuestionKind::Score { .. });
    QuestionMetrics {
        id: q.id.clone(),
        kind: q.kind.type_name().into(),
        classes,
        items: items.len() as u64,
        answered,
        failed,
        unreadable,
        labelled,
        scored,
        accuracy: ratio(hits, scored),
        mae: (is_score && scored > 0).then(|| abs_err / scored as f64),
        confusion,
        reliability: reliability(q, items, bins),
        curve: curve(items),
    }
}

#[cfg(test)]
mod tests;
