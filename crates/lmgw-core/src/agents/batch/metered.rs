//! The agent run's meter (container-runtime / agents design §4.5): a
//! [`TurnRunner`] that tallies what every turn's row was billed and priced
//! at. A child of `batch` since the chat-voice WP11 review (n4), which grew
//! it.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use crate::agent::{DeltaSink, TurnRunner};
use crate::error::GatewayError;
use crate::ir::{ChatRequest, Completion, Usage};
use crate::pricing::RowCost;

use super::meter::Meter;

/// A [`TurnRunner`] that keeps a running total of what the model spent.
///
/// The loop reports usage on its `Ok` return only, so a run that ended in a
/// transport error would price at zero however many turns it had already paid
/// for. Tallying at the call makes the number independent of how the loop
/// ended (§4.5). The cost is each turn's row's, priced on the route that
/// answered it: a turn the gate re-routed is not priced on the route the run
/// started on (billable-units design §7).
pub(super) struct MeteredRunner<'a> {
    inner: &'a dyn TurnRunner,
    /// What the turns' rows were billed: the usage and the sum of their
    /// costs, and the turns answered — its model calls. The loop's tool calls
    /// are the caller's, from its events.
    tally: Mutex<Meter>,
}

impl<'a> MeteredRunner<'a> {
    pub(super) fn new(inner: &'a dyn TurnRunner) -> Self {
        Self {
            inner,
            tally: Mutex::new(Meter::default()),
        }
    }

    /// The tally so far, without the loop's tool calls.
    pub(super) fn tally(&self) -> Meter {
        self.tally.lock().unwrap().clone()
    }
}

#[async_trait]
impl TurnRunner for MeteredRunner<'_> {
    async fn run_turn(
        &self,
        ir: &ChatRequest,
        deadline: Duration,
        sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError> {
        let mut billed = Billed {
            sink,
            usage: None,
            cost: None,
        };
        let out = self.inner.run_turn(ir, deadline, &mut billed).await;
        // What the turn's row was billed, a stopped or failed turn's too
        // (WP11 server review n4: a cancelled turn writes its row with the
        // cost so far, and the job's meter must not be lower than its
        // rows); a runner that does not say leaves its completion's usage.
        let usage = billed.usage.or_else(|| out.as_ref().ok().map(|c| c.usage));
        // What the row adds to a total, by the rollup's own rule
        // (`NewRequestLog::row_cost`): its cost, or — unpriced — a gap where
        // it did work: answered, stopped, or spent tokens or a quantity. A
        // runner that never said is judged by how the turn ended: answered or
        // stopped leaves the run unknown, a failure knows nothing to add.
        let row = billed.cost.unwrap_or_else(|| {
            let worked = out
                .as_ref()
                .map_or_else(crate::proxy::is_canceled, |_| true);
            RowCost::of(None, worked)
        });
        let mut tally = self.tally.lock().unwrap();
        if let Some(u) = usage {
            tally.usage.add(&u);
        }
        tally.add_cost(row);
        // A model call is an answer without an error, as the HTTP run meters
        // count one (`RunMeters::note_row`): a turn that failed or was
        // stopped is none, though its row's cost counts.
        tally.model_calls += u32::from(out.is_ok());
        out
    }
}

/// The caller's sink, noting what the turn's row was billed and adds to a
/// total (`DeltaSink::billed`, `DeltaSink::billed_cost`).
struct Billed<'s> {
    sink: &'s mut dyn DeltaSink,
    usage: Option<Usage>,
    /// `None`: never said.
    cost: Option<RowCost>,
}

impl DeltaSink for Billed<'_> {
    fn on_delta(&mut self, d: &crate::ir::StreamDelta) {
        self.sink.on_delta(d);
    }

    fn stop(&self) -> Option<crate::proxy::StopSignal> {
        self.sink.stop()
    }

    fn flush(&mut self) {
        self.sink.flush();
    }

    fn billed(&mut self, usage: &Usage) {
        self.usage = Some(*usage);
        self.sink.billed(usage);
    }

    fn billed_cost(&mut self, row: RowCost) {
        self.cost = Some(row);
        self.sink.billed_cost(row);
    }
}

#[cfg(test)]
mod tests;
