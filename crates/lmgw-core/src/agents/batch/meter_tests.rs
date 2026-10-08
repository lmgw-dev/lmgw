//! The in-process run meter sums what its calls' rows add to a total, each
//! priced on the route that answered it (billable-units design §7): a free
//! local call is a real 0 in the sum, unpriced work leaves the run unknown —
//! never the other calls' partial sum, also once folded into another meter —
//! and a run no row added a cost to is unpriced, not free. The HTTP half of a
//! run (`RunMeters`) folds in by the same rule.

use serde_json::{json, Value};

use super::Meter;
use crate::agents::RunMeters;
use crate::ir::Usage;
use crate::pricing::RowCost::{self, Nothing, Priced, Unpriced};

fn usage(prompt: u64, completion: u64) -> Usage {
    Usage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        ..Default::default()
    }
}

/// One call, as a classify call's tally comes back: counted as a model call
/// when `answered`.
fn call(row: RowCost, tokens: Usage, answered: bool) -> Meter {
    let mut m = Meter {
        usage: tokens,
        model_calls: u32::from(answered),
        ..Default::default()
    };
    m.add_cost(row);
    m
}

fn cost(meter: &Meter) -> Value {
    meter.report()["cost_micro"].clone()
}

#[test]
fn a_run_costs_the_sum_of_its_calls_rows() {
    let mut run = Meter::default();
    run.absorb(call(Priced(0), usage(1, 1), true));
    assert_eq!(cost(&run), json!(0), "a local call is a real 0");
    run.absorb(call(Priced(5_000), usage(2, 0), true));
    run.absorb(call(Priced(1_234), usage(3, 1), true));
    // A refusal that spent nothing adds nothing, and leaves no gap.
    run.absorb(call(Nothing, Usage::default(), false));
    let report = run.report();
    assert_eq!(report["cost_micro"], json!(6_234));
    assert_eq!(report["model_calls"], json!(3));
    assert_eq!(report["usage"]["prompt_tokens"], json!(6));
}

#[test]
fn unpriced_work_leaves_the_run_unknown_never_a_partial_sum() {
    let mut step = Meter::default();
    step.absorb(call(Priced(5_000), usage(1, 1), true));
    step.absorb(call(Unpriced, Usage::default(), true));
    assert_eq!(cost(&step), Value::Null);
    assert_eq!(
        step.report()["usage"]["prompt_tokens"],
        json!(1),
        "the tokens still show"
    );

    // Folded into another meter, the gap travels with it, and a later
    // priced call does not bring the sum back.
    let mut run = Meter::default();
    run.absorb(call(Priced(0), usage(1, 1), true));
    run.absorb(step);
    run.absorb(call(Priced(5_000), usage(1, 1), true));
    assert_eq!(cost(&run), Value::Null);
}

#[test]
fn a_run_no_priced_row_added_to_is_unpriced_not_free() {
    // Tool calls alone, and a refusal that spent nothing: no row added a
    // cost, so nothing is known — the NULL the HTTP run meters report too.
    let mut run = Meter::default();
    run.absorb(Meter {
        tool_calls: 3,
        ..Default::default()
    });
    run.absorb(call(Nothing, Usage::default(), false));
    assert_eq!(cost(&run), Value::Null);
    assert_eq!(run.report()["tool_calls"], json!(3));

    // A failed call on a local route is a real 0, model call or not.
    let mut local = run.clone();
    local.absorb(call(Priced(0), Usage::default(), false));
    assert_eq!(cost(&local), json!(0));

    // A meter with nothing in it adds nothing to one that has a cost.
    let mut priced = call(Priced(42), usage(1, 1), true);
    priced.absorb(run);
    assert_eq!(cost(&priced), json!(42));
}

/// The run's HTTP half folds in through `Meter::from(&RunTotals)` by the
/// same rule (review WP2b #4): a half no row added a cost to adds nothing,
/// whatever its call counts say, and a half with a cost counts even when it
/// made no model call.
#[test]
fn the_http_half_folds_in_by_what_its_rows_added() {
    const RUN: i64 = 7;
    let open = || {
        let m = RunMeters::default();
        m.open(RUN);
        m
    };
    let fold = |own: Meter, http: &RunMeters| {
        let mut m = own;
        m.absorb(Meter::from(&http.read(RUN)));
        m.report()
    };

    // In-process: one classify call that failed after its fee was paid, so
    // no model call; HTTP: one priced answer. Both costs count.
    let http = open();
    http.note_row(RUN, &usage(1, 1), Priced(50), true);
    let out = fold(call(Priced(100), usage(2, 0), false), &http);
    assert_eq!(out["cost_micro"], json!(150), "{out}");
    assert_eq!(out["model_calls"], json!(1));
    assert_eq!(out["usage"]["prompt_tokens"], json!(3));

    // In-process: a tool loop's turn that started and failed knowing
    // nothing (a model call by the loop's count, no cost). It does not
    // poison the container's priced call.
    let failed_turn = Meter {
        model_calls: 1,
        ..Default::default()
    };
    let out = fold(failed_turn.clone(), &http);
    assert_eq!(out["cost_micro"], json!(50), "{out}");
    assert_eq!(out["model_calls"], json!(2));

    // HTTP: only tool calls and a refusal — nothing added to the cost, so
    // the in-process cost stands.
    let tools = open();
    tools.note_tool_call(RUN);
    tools.note_row(RUN, &Usage::default(), Nothing, false);
    let out = fold(call(Priced(100), usage(1, 1), true), &tools);
    assert_eq!(out["cost_micro"], json!(100), "{out}");
    assert_eq!(out["tool_calls"], json!(1));
    assert_eq!(out["model_calls"], json!(1));

    // Either half unknown: the run is unknown.
    let unknown = open();
    unknown.note_row(RUN, &usage(1, 1), Unpriced, false);
    assert_eq!(
        fold(call(Priced(100), usage(1, 1), true), &unknown)["cost_micro"],
        Value::Null
    );
    assert_eq!(
        fold(call(Unpriced, usage(1, 1), true), &http)["cost_micro"],
        Value::Null
    );
    // Neither half added anything: unpriced, not free.
    assert_eq!(fold(failed_turn, &tools)["cost_micro"], Value::Null);
}
