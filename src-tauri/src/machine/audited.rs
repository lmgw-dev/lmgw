//! `machine_apply`'s audit entry (headless server design §5.8, D93): with
//! accounts on, the plan it ran and how it ended, as account 1 at the
//! machine (`via = app`).

use lmgw_api_types::machine::{MachineApplyOutcome as O, MachineApplyResult};
use lmgw_core::state::SharedState;

/// The answer's status the entry carries for `outcome`.
fn status(outcome: O) -> u16 {
    match outcome {
        O::Applied | O::Updated | O::NothingToDo => 200,
        O::Declined => 403,
        O::Unavailable => 503,
        O::PlanChanged | O::ReadDifferently => 409,
        _ => 500,
    }
}

/// Write the entry for the call on plan `hash` that answered `result`.
pub(super) async fn record(state: &SharedState, hash: &str, result: &MachineApplyResult) {
    let target = serde_json::Map::from_iter([
        ("plan".to_string(), hash.into()),
        (
            "result".to_string(),
            serde_json::to_value(result.outcome).unwrap_or_default(),
        ),
    ]);
    lmgw_core::audit::desktop(
        state,
        lmgw_core::audit::Via::App,
        "machine_apply",
        target,
        status(result.outcome),
    )
    .await;
}
