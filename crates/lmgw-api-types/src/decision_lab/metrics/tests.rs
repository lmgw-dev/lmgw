//! The one rule of what an answer predicts and how a run reads, which the
//! server's metrics, readable judgement and export and the page's items
//! table all call.

use super::*;
use crate::decision_lab::examples;

/// Ticket triage: `team` (choice), `urgency` (score of four), `needs_human`
/// (noul).
fn triage() -> Vec<QuestionDraft> {
    examples().into_iter().next().unwrap().form.questions
}

fn answer(json: &str) -> Answer {
    serde_json::from_str(json).unwrap()
}

fn response(answers: &str) -> String {
    format!(
        r#"{{"model": "m", "answers": {answers}, "usage": {{"input_tokens": 1, "output_tokens": 0}}}}"#
    )
}

#[test]
fn a_choice_predicts_its_winner_and_refuses_one_the_question_lacks() {
    let qs = triage();
    let a = answer(
        r#"{"type": "choice", "choice": "billing", "probabilities": {"billing": 0.81, "technical": 0.19}, "confidence": 0.7}"#,
    );
    let p = predict(&qs[0], &a).unwrap();
    assert_eq!(
        (p.class.as_str(), p.probability, p.confidence, p.score),
        ("billing", 0.81, 0.7, None)
    );
    assert_eq!(correct(&qs[0], &a, "billing"), Some(true));
    assert_eq!(correct(&qs[0], &a, "technical"), Some(false));
    let stray = answer(
        r#"{"type": "choice", "choice": "sales", "probabilities": {"sales": 1.0}, "confidence": 1.0}"#,
    );
    assert_eq!(predict(&qs[0], &stray), None, "a winner it does not offer");
    // The winner's probability missing: nothing to bin it by.
    let bare = answer(
        r#"{"type": "choice", "choice": "billing", "probabilities": {"technical": 0.2}, "confidence": 0.6}"#,
    );
    assert_eq!(predict(&qs[0], &bare), None);
}

#[test]
fn a_score_predicts_its_nearest_level_which_needs_its_probability() {
    let qs = triage();
    let s = answer(
        r#"{"type": "score", "score": 2.4, "legend": {}, "probabilities": {"0": 0.1, "1": 0.1, "2": 0.5, "3": 0.3}, "confidence": 0.5}"#,
    );
    let p = predict(&qs[1], &s).unwrap();
    assert_eq!(
        (p.class.as_str(), p.probability, p.score),
        ("2", 0.5, Some(2.4))
    );
    // Past the last level: clamped to it.
    let high = answer(
        r#"{"type": "score", "score": 7.0, "legend": {}, "probabilities": {"3": 0.9}, "confidence": 0.9}"#,
    );
    assert_eq!(predict(&qs[1], &high).unwrap().class, "3");
    let no_prob = answer(
        r#"{"type": "score", "score": 1.2, "legend": {}, "probabilities": {"0": 0.5}, "confidence": 0.5}"#,
    );
    assert_eq!(
        predict(&qs[1], &no_prob),
        None,
        "level 1 has no probability"
    );
    assert_eq!(predict(&qs[2], &s), None, "another type is no answer");
}

#[test]
fn a_noul_predicts_yes_from_one_half_and_cuts_on_its_certainty() {
    let qs = triage();
    let n = |p: f64| answer(&format!(r#"{{"type": "noul", "noul": {p}}}"#));
    let p = predict(&qs[2], &n(0.3)).unwrap();
    assert_eq!(
        (p.class.as_str(), p.probability, p.confidence),
        ("no", 0.3, 0.7)
    );
    assert_eq!(predict(&qs[2], &n(0.5)).unwrap().class, "yes");
    assert_eq!(certainty(0.82), 0.82);
    assert_eq!(classes(&qs[2]), ["yes", "no"]);
    assert_eq!(classes(&qs[1]), ["0", "1", "2", "3"]);
}

#[test]
fn a_run_reads_pending_failed_unreadable_or_answered_on_the_2xx_range() {
    let qs = triage();
    let text = response(
        r#"{"team": {"type": "choice", "choice": "billing", "probabilities": {"billing": 0.9}, "confidence": 0.8}, "needs_human": {"type": "score", "score": 1.0, "legend": {}, "probabilities": {}, "confidence": 0.1}}"#,
    );
    let read = |status: Option<i64>| read_response(status, Some(&text));
    assert_eq!(outcome(&qs[0], None, None), Outcome::Pending);
    for failed in [199, 300, 404, 429, 500, 503] {
        assert_eq!(read(Some(failed)), None, "{failed}: not read");
        assert_eq!(
            outcome(&qs[0], Some(failed), read(Some(failed)).as_ref()),
            Outcome::Failed,
            "{failed}"
        );
    }
    // Any 2xx reads; 200 is not special.
    for ok in [200, 203, 299] {
        let r = read(Some(ok));
        assert!(matches!(
            outcome(&qs[0], Some(ok), r.as_ref()),
            Outcome::Answered(_)
        ));
    }
    let r = read(Some(200));
    assert_eq!(
        outcome(&qs[2], Some(200), r.as_ref()),
        Outcome::Unreadable,
        "a score answer to a noul question"
    );
    assert_eq!(
        outcome(&qs[1], Some(200), r.as_ref()),
        Outcome::Unreadable,
        "no answer at all"
    );
    // A 2xx whose text is no answer is unreadable, not failed.
    assert_eq!(read_response(Some(200), Some("not json")), None);
    assert_eq!(outcome(&qs[0], Some(200), None), Outcome::Unreadable);
    assert_eq!(read_response(Some(200), None), None);
}
