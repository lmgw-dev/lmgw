//! A row of a call stamped `X-Lmgw-Run` folds into its run by the rollup's
//! own rule (review WP2b #2, `NewRequestLog::row_cost`): a stopped call's
//! priced row and a failed call's fee count, a failure that spent tokens no
//! one could price leaves the run unknown — as it leaves a gap in Usage — and
//! a refusal that spent nothing, a tool row and a row routed nowhere add
//! nothing. Only an answer without an error is a model call.

use super::note_on_run;
use crate::ir::Usage;
use crate::pricing::Cost;
use crate::state::AppState;
use crate::store::NewRequestLog;
use crate::telemetry::RequestClass;

const RUN: i64 = 3;

fn row(status: i64, error: Option<&str>, prompt: Option<i64>, cost: Option<i64>) -> NewRequestLog {
    NewRequestLog {
        upstream_id: Some(5),
        status,
        error_kind: error.map(str::to_string),
        prompt_tokens: prompt,
        cost: Cost {
            total_micro: cost,
            ..Cost::unknown()
        },
        ..Default::default()
    }
}

fn usage(r: &NewRequestLog) -> Usage {
    Usage {
        prompt_tokens: r.prompt_tokens.map(|v| v as u64),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_stamped_row_folds_into_its_run_by_the_rollups_rule() {
    let state = AppState::init_for_tests().await.unwrap();
    // Open, as the run's job going live opens it.
    state.agent_meters.open(RUN);
    let fold = |r: NewRequestLog| note_on_run(&state, Some(RUN), &r, &usage(&r));

    fold(row(200, None, Some(10), Some(100)));
    fold(row(200, Some("canceled"), Some(6), Some(40)));
    fold(row(502, Some("upstream"), Some(3), Some(5)));
    // A refusal that spent nothing, a tool row, a row routed nowhere.
    fold(row(503, Some("gpu_hold"), None, None));
    fold(NewRequestLog {
        class: RequestClass::Tool,
        ..row(200, None, None, None)
    });
    fold(NewRequestLog {
        upstream_id: None,
        ..row(200, None, Some(9), None)
    });
    // Not stamped: nothing.
    note_on_run(
        &state,
        None,
        &row(200, None, Some(1), None),
        &Usage::default(),
    );
    let t = state.agent_meters.read(RUN);
    assert_eq!(t.cost_micro(), Some(145), "{t:?}");
    assert_eq!(t.model_calls, 1, "only the answer");
    assert_eq!(t.usage.prompt_tokens, Some(19));

    // A failure mid-stream that spent tokens on an unpriced route.
    fold(row(502, Some("upstream"), Some(40), None));
    let t = state.agent_meters.read(RUN);
    assert_eq!(t.cost_micro(), None, "not the 145 of the others: {t:?}");
    assert_eq!(t.model_calls, 1);
}
