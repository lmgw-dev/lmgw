//! A run's cost over the calls it made over HTTP (`RunMeters`, agents §3.1):
//! the sum of what every call's row adds to a total, by the rollup's own rule
//! (`NewRequestLog::row_cost`), and unknown once any row was unpriced work.
//! Unknown is NULL, never a partial sum (`pricing::sum_micro`): the sum of the
//! priced calls alone would read as the whole run's cost. How the HTTP half
//! folds into an in-process run's report is `agents::batch`'s (`meter_tests`).

use lmgw_core::agents::{RunMeters, RunTotals};
use lmgw_core::ir::Usage;
use lmgw_core::pricing::RowCost::{Nothing, Priced, Unpriced};

const RUN: i64 = 7;

/// Meters with `RUN` open, as its job going live opens it.
fn open() -> RunMeters {
    let m = RunMeters::default();
    m.open(RUN);
    m
}

fn tokens(prompt: u64, completion: u64) -> Usage {
    Usage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        ..Default::default()
    }
}

#[test]
fn priced_calls_add_up_and_free_ones_are_a_real_zero() {
    let m = open();
    assert_eq!(
        m.read(RUN).cost_micro(),
        None,
        "no call yet: nothing priced"
    );
    m.note_row(RUN, &tokens(10, 5), Priced(100), true);
    m.note_row(RUN, &tokens(20, 5), Priced(50), true);
    let t = m.read(RUN);
    assert_eq!(t.cost_micro(), Some(150));
    assert_eq!(t.model_calls, 2);
    assert_eq!(t.usage.prompt_tokens, Some(30));

    let free = open();
    free.note_row(RUN, &tokens(1, 1), Priced(0), true);
    free.note_row(RUN, &tokens(1, 1), Priced(0), true);
    assert_eq!(free.read(RUN).cost_micro(), Some(0), "free, not unknown");
}

#[test]
fn one_unpriced_call_makes_the_run_unknown_for_good() {
    let m = open();
    m.note_row(RUN, &tokens(10, 5), Priced(100), true);
    m.note_row(RUN, &Usage::default(), Unpriced, true);
    assert_eq!(
        m.read(RUN).cost_micro(),
        None,
        "not the 100 of the priced call"
    );
    m.note_row(RUN, &tokens(10, 5), Priced(100), true);
    let t = m.take(RUN);
    assert_eq!(
        t.cost_micro(),
        None,
        "a later priced call does not bring it back"
    );
    assert_eq!(t.model_calls, 3, "the unpriced call still counts as a call");

    let first = open();
    first.note_row(RUN, &Usage::default(), Unpriced, true);
    first.note_row(RUN, &tokens(10, 5), Priced(100), true);
    assert_eq!(
        first.read(RUN).cost_micro(),
        None,
        "unpriced first, then priced"
    );
}

/// Review WP2b #2: a failed or stopped call's row counts by what it adds
/// to a total, not by whether it was an error. Only an answer is a model
/// call.
#[test]
fn a_failed_call_adds_what_its_row_adds_and_is_no_model_call() {
    // A stopped stream's priced tokens, and a failure's fee: both count.
    let m = open();
    m.note_row(RUN, &tokens(10, 5), Priced(100), true);
    m.note_row(RUN, &tokens(4, 2), Priced(30), false);
    let t = m.read(RUN);
    assert_eq!(t.cost_micro(), Some(130));
    assert_eq!(t.model_calls, 1);
    assert_eq!(t.usage.prompt_tokens, Some(14), "its tokens are the run's");

    // A refusal that spent nothing leaves no gap.
    m.note_row(RUN, &Usage::default(), Nothing, false);
    assert_eq!(m.read(RUN).cost_micro(), Some(130));

    // A failure mid-stream on an unpriced route, having spent tokens, is a
    // gap in Usage — and so in the run: not the 130 of the others.
    m.note_row(RUN, &tokens(40, 9), Unpriced, false);
    let t = m.read(RUN);
    assert_eq!(t.cost_micro(), None);
    assert_eq!(t.model_calls, 1);
}

#[test]
fn a_run_of_refusals_and_tool_calls_is_unpriced_not_free() {
    let m = open();
    m.note_tool_call(RUN);
    m.note_row(RUN, &Usage::default(), Nothing, false);
    let t = m.read(RUN);
    assert!(t.cost.is_empty(), "nothing added to the cost");
    assert_eq!(t.cost_micro(), None);
    assert_eq!((t.model_calls, t.tool_calls), (0, 1));
}

/// Review WP2b, second pass: stamped work that finishes after its run was
/// taken — a realtime session, a long `/v1/responses` loop — re-creates no
/// entry nobody would take. Nor does a run that was never opened get one.
#[test]
fn a_call_for_a_run_that_is_not_open_is_dropped() {
    let m = open();
    m.note_row(RUN, &tokens(1, 1), Priced(10), true);
    assert_eq!(m.take(RUN).cost_micro(), Some(10));

    m.note_row(RUN, &tokens(1, 1), Priced(10), true);
    m.note_tool_call(RUN);
    assert_eq!(m.take(RUN), RunTotals::default(), "nothing re-created");

    let never = RunMeters::default();
    never.note_row(RUN, &tokens(1, 1), Priced(10), true);
    assert_eq!(never.read(RUN), RunTotals::default());
}
