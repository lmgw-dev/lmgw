//! The in-process run's meter (agents design §4.5): what a run spent, summed
//! as it goes from what its calls' rows add to a total.

use serde_json::{json, Value};

use crate::agents::RunTotals;
use crate::ir::Usage;
use crate::pricing::{CostTotal, RowCost};

/// What a run spent. The cost is the **sum of its calls' row costs**, each
/// priced on the route that answered it, under the rollup's own rule for an
/// unpriced row ([`crate::store::NewRequestLog::row_cost`]) — as the HTTP run
/// meters sum theirs ([`RunTotals`]): a call the gate re-routed — a ladder's
/// fallback, a candidate alias's next pick — is priced on its own route,
/// never on the one the run started on (billable-units design §7).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Meter {
    /// Plain statistics, shown on the run: the tokens its calls reported, and
    /// how many model and tool calls it made.
    pub(super) usage: Usage,
    pub(super) model_calls: u32,
    pub(super) tool_calls: u32,
    /// What the calls' rows add up to: nothing until a row adds to it, then
    /// their sum, unknown for good once a row was unpriced work.
    pub(super) cost: CostTotal,
}

impl Meter {
    /// One call's row.
    pub(super) fn add_cost(&mut self, row: RowCost) {
        self.cost.add(row);
    }

    /// The §4.5 block.
    pub(super) fn report(&self) -> Value {
        json!({
            "usage": self.usage,
            // NULL, never 0, when nothing could be priced — a run no priced
            // row added to included: "we do not know" and "it was free" are
            // different answers (usage-analytics §2.2).
            "cost_micro": self.cost.micro(),
            "model_calls": self.model_calls,
            "tool_calls": self.tool_calls,
        })
    }

    pub(super) fn absorb(&mut self, other: Meter) {
        self.usage.add(&other.usage);
        self.model_calls += other.model_calls;
        self.tool_calls += other.tool_calls;
        self.cost.absorb(other.cost);
    }
}

/// The calls a run's container made over HTTP, stamped `X-Lmgw-Run` (§3.1):
/// folded in with [`Meter::absorb`], so one run that both drove model turns
/// itself and had a container call `/v1` reports one number by one rule — a
/// half no row added a cost to adds nothing, whatever its call counts say.
impl From<&RunTotals> for Meter {
    fn from(t: &RunTotals) -> Self {
        Self {
            usage: t.usage,
            model_calls: t.model_calls,
            tool_calls: t.tool_calls,
            cost: t.cost,
        }
    }
}
