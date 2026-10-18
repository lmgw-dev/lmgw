use lmgw_api_types::decision_lab::OptionDraft;
use lmgw_api_types::systemone::{Answer, SystemOneResponse};

use super::*;

fn choice() -> QuestionDraft {
    QuestionDraft {
        id: "team".into(),
        instructions: String::new(),
        kind: QuestionKind::Choice {
            options: ["billing", "technical", "other"]
                .map(|n| OptionDraft {
                    name: n.into(),
                    description: String::new(),
                })
                .to_vec(),
        },
    }
}

fn answer(json: &str) -> Answer {
    serde_json::from_str(json).unwrap()
}

/// A `choice` answer: `winner` with probability `p`, confidence `conf`.
fn picked(winner: &str, p: f64, conf: f64) -> Answer {
    let rest = (1.0 - p) / 2.0;
    let probs: Vec<String> = ["billing", "technical", "other"]
        .iter()
        .map(|o| format!("\"{o}\": {}", if *o == winner { p } else { rest }))
        .collect();
    answer(&format!(
        r#"{{"type": "choice", "choice": "{winner}", "probabilities": {{{}}}, "confidence": {conf}}}"#,
        probs.join(", ")
    ))
}

fn item(q: &QuestionDraft, label: Option<&str>, a: Option<Answer>) -> ItemOutcome {
    ItemOutcome {
        expected: label.map(String::from),
        outcome: match a {
            Some(a) => predict(q, &a).map_or(Outcome::Failed, Outcome::Answered),
            None => Outcome::Failed,
        },
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// Six items: three labelled answers (two right), an unlabelled answer, a
/// failed run and one not run yet.
fn choice_items() -> Vec<ItemOutcome> {
    let q = choice();
    vec![
        item(&q, Some("billing"), Some(picked("billing", 0.92, 0.85))),
        item(&q, Some("billing"), Some(picked("technical", 0.65, 0.4))),
        item(&q, None, Some(picked("billing", 0.7, 0.6))),
        item(&q, Some("technical"), None),
        ItemOutcome {
            expected: Some("other".into()),
            outcome: Outcome::Pending,
        },
        item(&q, Some("other"), Some(picked("other", 0.97, 0.9))),
    ]
}

#[test]
fn a_choice_is_right_when_its_winner_is_the_label() {
    let m = question(&choice(), &choice_items(), 10);
    assert_eq!(m.kind, "choice");
    assert_eq!(m.classes, ["billing", "technical", "other"]);
    assert_eq!(
        (
            m.items,
            m.answered,
            m.failed,
            m.unreadable,
            m.labelled,
            m.scored
        ),
        (6, 4, 1, 0, 5, 3)
    );
    assert!(close(m.accuracy.unwrap(), 2.0 / 3.0), "{:?}", m.accuracy);
    assert_eq!(m.mae, None, "only a score has one");
    // Rows the labels, columns the predictions.
    assert_eq!(
        m.confusion,
        vec![vec![1, 1, 0], vec![0, 0, 0], vec![0, 0, 1]]
    );
}

#[test]
fn reliability_has_only_the_bins_that_hold_a_labelled_answer() {
    let q = choice();
    let bins = reliability(&q, &choice_items(), 10);
    // 0.65 (wrong) in bin 6; 0.92 and 0.97 (both right) in bin 9; the
    // unlabelled 0.7 is in none, and no other bin is listed.
    let held: Vec<(u32, u64)> = bins.iter().map(|b| (b.index, b.count)).collect();
    assert_eq!(held, [(6, 1), (9, 2)]);
    assert!(close(bins[0].lower, 0.6) && close(bins[0].upper, 0.7));
    assert_eq!(bins[0].observed, 0.0);
    assert!(close(bins[0].mean_prediction, 0.65));
    assert!(close(bins[1].mean_prediction, 0.945));
    assert_eq!(bins[1].observed, 1.0);
    // One bin takes everything; 1.0 falls in the last.
    let one = reliability(&q, &choice_items(), 1);
    assert_eq!((one.len(), one[0].index, one[0].count), (1, 0, 3));
    let sure = [item(&q, Some("billing"), Some(picked("billing", 1.0, 1.0)))];
    let last = reliability(&q, &sure, 4);
    assert_eq!((last[0].index, last[0].count), (3, 1));
    // Four billion bins cost what one answer does.
    let many = reliability(&q, &sure, 4_000_000_000);
    assert_eq!(many.len(), 1);
    assert_eq!(many[0].index, 3_999_999_999);
    assert!(many[0].upper == 1.0 && many[0].lower < 1.0);
}

#[test]
fn an_answer_is_binned_by_the_edges_the_bins_report() {
    // floor(0.57 × 100) is 56 in f64; the reported edges put it at 57.
    assert_eq!((0.57f64 * 100.0).floor(), 56.0);
    assert_eq!(bin_of(0.57, 100), 57);
    for (p, bins) in [
        (0.57, 100),
        (0.29, 100),
        (0.1, 10),
        (0.3, 10),
        (0.7, 10),
        (0.8, 4_000_000_000),
        (1.0 / 3.0, 3),
        (0.0, 7),
        (0.999_999, 1_000_000),
    ] {
        let k = bin_of(p, bins);
        let (lower, upper) = (
            f64::from(k) / f64::from(bins),
            f64::from(k + 1) / f64::from(bins),
        );
        assert!(
            lower <= p && p < upper,
            "{p} in bin {k} [{lower}, {upper}) of {bins}"
        );
    }
    assert_eq!(bin_of(1.0, 10), 9, "the last bin holds 1");
    assert_eq!(bin_of(1.5, 10), 9);
    assert_eq!(bin_of(-0.2, 10), 0);
}

#[test]
fn the_curve_has_a_point_per_distinct_confidence_lowest_first() {
    let c = curve(&choice_items());
    let at: Vec<(f64, u64, u64, u64)> = c
        .iter()
        .map(|p| (p.threshold, p.covered, p.labelled, p.correct))
        .collect();
    assert_eq!(
        at,
        [
            (0.4, 4, 3, 2),
            (0.6, 3, 2, 2),
            (0.85, 2, 2, 2),
            (0.9, 1, 1, 1)
        ]
    );
    assert!(close(c[0].coverage, 1.0) && close(c[1].coverage, 0.75));
    assert!(close(c[0].accuracy.unwrap(), 2.0 / 3.0));
    assert_eq!(c[3].accuracy, Some(1.0));

    // A cut between two points keeps what the higher one keeps.
    let p = at_threshold(&choice_items(), 0.7);
    assert_eq!((p.covered, p.labelled, p.correct), (2, 2, 2));
    assert!(close(p.coverage, 0.5));
    // Equal confidences are one point.
    let q = choice();
    let twice = [
        item(&q, Some("billing"), Some(picked("billing", 0.8, 0.5))),
        item(&q, Some("other"), Some(picked("billing", 0.8, 0.5))),
    ];
    let c = curve(&twice);
    assert_eq!(c.len(), 1);
    assert_eq!((c[0].covered, c[0].correct), (2, 1));
    assert!(curve(&[]).is_empty());
}

#[test]
fn a_noul_predicts_yes_from_one_half_and_is_cut_on_its_certainty() {
    let q = QuestionDraft {
        id: "angry".into(),
        instructions: String::new(),
        kind: QuestionKind::Noul {
            yes: String::new(),
            no: String::new(),
        },
    };
    let noul = |p: f64| answer(&format!(r#"{{"type": "noul", "noul": {p}}}"#));
    let items = [
        item(&q, Some("yes"), Some(noul(0.8))),
        item(&q, Some("yes"), Some(noul(0.3))),
        item(&q, Some("no"), Some(noul(0.5))),
    ];
    let m = question(&q, &items, 2);
    assert_eq!(m.classes, ["yes", "no"]);
    assert!(close(m.accuracy.unwrap(), 1.0 / 3.0));
    assert_eq!(m.confusion, vec![vec![1, 1], vec![1, 0]]);
    let thresholds: Vec<f64> = m.curve.iter().map(|p| p.threshold).collect();
    assert_eq!(thresholds, [0.5, 0.7, 0.8], "max(p, 1 − p)");
    // The standard reliability diagram: P(yes) against the rate of yes
    // labels. 0.3 (labelled yes) in the lower bin; 0.8 (yes) and 0.5 (no)
    // in the upper.
    let bins: Vec<(u32, u64)> = m.reliability.iter().map(|b| (b.index, b.count)).collect();
    assert_eq!(bins, [(0, 1), (1, 2)]);
    assert!(close(m.reliability[0].mean_prediction, 0.3));
    assert_eq!(m.reliability[0].observed, 1.0);
    assert!(close(m.reliability[1].mean_prediction, 0.65));
    assert_eq!(m.reliability[1].observed, 0.5);
}

#[test]
fn a_score_is_right_at_its_nearest_level_and_measures_its_distance() {
    let q = QuestionDraft {
        id: "urgency".into(),
        instructions: String::new(),
        kind: QuestionKind::Score {
            levels: ["week", "days", "today", "now"].map(String::from).to_vec(),
        },
    };
    let score = |s: f64, conf: f64| {
        answer(&format!(
            r#"{{"type": "score", "score": {s}, "legend": {{"0": "week", "1": "days",
                "2": "today", "3": "now"}}, "probabilities": {{"0": 0.07, "1": 0.29,
                "2": 0.51, "3": 0.13}}, "confidence": {conf}}}"#
        ))
    };
    let items = [
        item(&q, Some("2"), Some(score(1.68, 0.43))),
        item(&q, Some("1"), Some(score(0.4, 0.2))),
        // Past the scale: the nearest level is the last.
        item(&q, Some("3"), Some(score(3.4, 0.9))),
    ];
    let m = question(&q, &items, 10);
    assert_eq!(m.classes, ["0", "1", "2", "3"]);
    assert!(close(m.accuracy.unwrap(), 2.0 / 3.0));
    assert!(
        close(m.mae.unwrap(), (0.32 + 0.6 + 0.4) / 3.0),
        "{:?}",
        m.mae
    );
    assert_eq!(m.confusion[2][2], 1);
    assert_eq!(m.confusion[1][0], 1);
    assert_eq!(m.confusion[3][3], 1);
    // The nearest level's probability: level 2's 0.51 for 1.68.
    match predict(&q, &score(1.68, 0.43)) {
        Some(p) => assert!(close(p.probability, 0.51) && close(p.confidence, 0.43)),
        None => panic!("a score answer"),
    }
}

#[test]
fn an_outcome_is_pending_failed_or_a_readable_answer_of_the_questions_type() {
    let q = choice();
    let resp: SystemOneResponse = serde_json::from_str(
        r#"{"model": "m", "answers": {"team": {"type": "choice", "choice": "billing",
            "probabilities": {"billing": 0.9, "technical": 0.05, "other": 0.05},
            "confidence": 0.8}, "angry": {"type": "noul", "noul": 0.2}},
            "usage": {"input_tokens": 1, "output_tokens": 0}}"#,
    )
    .unwrap();
    assert_eq!(outcome(&q, None, None), Outcome::Pending);
    assert_eq!(outcome(&q, Some(500), None), Outcome::Failed);
    assert_eq!(
        outcome(&q, Some(200), None),
        Outcome::Unreadable,
        "no answer"
    );
    assert!(matches!(
        outcome(&q, Some(200), Some(&resp)),
        Outcome::Answered(_)
    ));
    let mut other = q.clone();
    other.id = "angry".into();
    assert_eq!(
        outcome(&other, Some(200), Some(&resp)),
        Outcome::Unreadable,
        "a noul answer to a choice question"
    );
    other.id = "missing".into();
    assert_eq!(outcome(&other, Some(200), Some(&resp)), Outcome::Unreadable);
    // Counted apart from the failed runs.
    let m = question(
        &other,
        &[
            ItemOutcome {
                expected: None,
                outcome: outcome(&other, Some(200), Some(&resp)),
            },
            ItemOutcome {
                expected: None,
                outcome: outcome(&other, Some(503), None),
            },
        ],
        10,
    );
    assert_eq!((m.failed, m.unreadable, m.answered), (1, 1, 0));
    assert_eq!(
        correct(&q, resp.answers.get("team").unwrap(), "billing"),
        Some(true)
    );
    // A winner the set does not offer is no answer of it.
    assert_eq!(predict(&q, &picked("sales", 0.5, 0.5)), None);
}
