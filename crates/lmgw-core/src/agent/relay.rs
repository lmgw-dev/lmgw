//! One model turn of the loop, relayed as it arrives (chat-voice design §7.2).
//!
//! [`TurnRunner::run_turn`] hands its deltas to a synchronous [`DeltaSink`];
//! the loop's [`EventSink`] is async. So the sink a turn gets here forwards
//! every delta into an unbounded channel, and the loop `select!`s between the
//! turn future and that channel. `Text`, `Reasoning`, `CallStarted` and
//! `CallArgs` go out the moment their delta does, so a tool thread streams
//! token by token and its time to first token is real. Arrival order is kept:
//! a call's run-global index is handed out when its start arrives, as the
//! replay after the turn used to.
//!
//! What bounds the channel is one poll of the turn, not the turn: the loop
//! drains the channel before it polls the turn again (`biased`), and while an
//! emit waits on a slow reader the turn is not polled at all. A stalled
//! client therefore holds the upstream back instead of filling memory.
//!
//! **A turn that ends early is stopped, then awaited**, when its runner took
//! the stop ([`DeltaSink::stop`]); the in-process model calls all do. The
//! turn's sink carries a [`StopSignal`](crate::proxy::StopSignal). It is
//! raised when the sink gives up mid-turn or the run's cancel lands, and the
//! turn then ends at its next await and still writes its `request_logs` row:
//! status 200, `canceled`, and what it cost so far (`proxy/stop.rs`). A
//! dropped turn would write no row, so a client could take a streamed answer
//! budget-free by hanging up. A runner that never took the stop (a unary one)
//! is dropped, as before, which closes its upstream request.
//!
//! The cancel is raced against every emit as well, so a reader that stopped
//! reading cannot hold a cancelled turn (and the GPU admission its runner
//! carries) in place. Nothing queued after the cancel is relayed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use super::{Cancel, DeltaSink, EventSink, LoopEvent, ResolvedTool, TurnAccumulator, TurnRunner};
use crate::error::GatewayError;
use crate::ir::{ChatRequest, Completion, StreamDelta};
use crate::proxy::StopSignal;

/// Who owns each of the run's tools, by name.
pub(super) struct Owners<'a>(HashMap<&'a str, &'a ResolvedTool>);

impl<'a> Owners<'a> {
    pub fn new(tools: &'a [ResolvedTool]) -> Self {
        Self(tools.iter().map(|t| (t.def.name.as_str(), t)).collect())
    }

    /// The server label of the tool `name`, `None` for a client's tool.
    pub fn label_of(&self, name: &str) -> Option<String> {
        self.0
            .get(name)
            .and_then(|t| t.server_label.as_deref())
            .map(str::to_string)
    }

    /// Whether the caller gated the tool `name` behind an approval.
    pub fn gated(&self, name: &str) -> bool {
        self.0.get(name).is_some_and(|t| t.needs_approval)
    }
}

/// How a relayed turn ended.
pub(super) enum TurnEnd {
    /// The runner returned.
    Returned(Box<Returned>),
    /// The run's cancel abandoned the turn (stopped or dropped, module doc).
    Canceled,
    /// The sink gave up mid-turn (the turn was stopped or dropped).
    SinkGone,
}

/// A turn the runner returned: what it returned, and `acc`, which folded
/// every delta it streamed. `streamed` is false for a unary runner, which
/// streamed none — its completion is the turn.
pub(super) struct Returned {
    pub out: Result<Completion, GatewayError>,
    pub acc: TurnAccumulator,
    pub streamed: bool,
}

/// The turn's [`DeltaSink`]: every delta into the loop's channel, and the
/// turn's stop for a runner that asks for it.
struct Forward<'t> {
    tx: mpsc::UnboundedSender<StreamDelta>,
    stop: StopSignal,
    /// Set when the runner asked for the stop. A runner that asks has
    /// promised to honour it, so the loop may await the turn once it is
    /// raised.
    took: &'t AtomicBool,
}

impl DeltaSink for Forward<'_> {
    fn on_delta(&mut self, d: &StreamDelta) {
        // The receiver lives until the turn ends; a send can only fail after
        // the loop stopped listening, when nothing is relayed anyway.
        let _ = self.tx.send(d.clone());
    }

    fn stop(&self) -> Option<StopSignal> {
        self.took.store(true, Ordering::Relaxed);
        Some(self.stop.clone())
    }
}

/// What the relay keeps across one turn's deltas.
struct Relay<'o, 'a> {
    acc: TurnAccumulator,
    streamed: bool,
    owners: &'o Owners<'a>,
    /// The run-global index of the first call this turn makes.
    next_index: usize,
    /// Calls started so far this turn. Counted per start, as
    /// [`TurnAccumulator`] gives each start its slot, so the indices
    /// handed out here are the ones the loop's `CallReady` uses.
    started: usize,
    /// The upstream's call ordinal → its run-global index.
    calls: HashMap<usize, usize>,
}

impl Relay<'_, '_> {
    /// Fold `d` into the turn and emit what it says. `false`: the sink gave
    /// up.
    async fn pass(&mut self, d: &StreamDelta, sink: &mut dyn EventSink) -> bool {
        self.acc.on_delta(d);
        self.streamed = true;
        let ev = match d {
            StreamDelta::TextDelta(t) if !t.is_empty() => LoopEvent::Text(t.clone()),
            StreamDelta::ReasoningDelta(r) if !r.is_empty() => LoopEvent::Reasoning(r.clone()),
            StreamDelta::ToolCallStart { index, id, name } => {
                let global = self.next_index + self.started;
                self.started += 1;
                self.calls.insert(*index, global);
                LoopEvent::CallStarted {
                    index: global,
                    call_id: id.clone(),
                    name: name.clone(),
                    server_label: self.owners.label_of(name),
                    needs_approval: self.owners.gated(name),
                }
            }
            StreamDelta::ToolCallArgsDelta { index, fragment } => match self.calls.get(index) {
                Some(global) => LoopEvent::CallArgs {
                    index: *global,
                    fragment: fragment.clone(),
                },
                None => return true,
            },
            _ => return true,
        };
        sink.emit(ev).await
    }
}

/// Run one model turn, relaying its deltas to `sink` as they arrive (module
/// doc). `next_index` is the run-global index of the turn's first call.
pub(super) async fn turn(
    runner: &dyn TurnRunner,
    ir: &ChatRequest,
    remaining: Duration,
    cancel: &Cancel,
    sink: &mut dyn EventSink,
    owners: &Owners<'_>,
    next_index: usize,
) -> TurnEnd {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop, signal) = crate::proxy::stop_pair();
    let took = AtomicBool::new(false);
    let mut forward = Forward {
        tx,
        stop: signal,
        took: &took,
    };
    let mut relay = Relay {
        acc: TurnAccumulator::default(),
        streamed: false,
        owners,
        next_index,
        started: 0,
        calls: HashMap::new(),
    };
    let run = runner.run_turn(ir, remaining, &mut forward);
    tokio::pin!(run);
    // The turn ends here, before its runner returned: stopped and awaited
    // when the runner took the stop, so its call writes its row; dropped
    // otherwise, which closes its upstream request (module doc).
    macro_rules! end_early {
        ($end:expr) => {{
            if took.load(Ordering::Relaxed) {
                stop.stop();
                let _ = (&mut run).await;
            }
            return $end;
        }};
    }
    loop {
        tokio::select! {
            biased;
            // This is the await that lasts minutes, so the cancel is raced
            // against it rather than checked after it.
            () = cancel.raised() => end_early!(TurnEnd::Canceled),
            Some(d) = rx.recv() => match cancel.guard(relay.pass(&d, sink)).await {
                Some(true) => {}
                Some(false) => end_early!(TurnEnd::SinkGone),
                None => end_early!(TurnEnd::Canceled),
            },
            out = &mut run => {
                // What the turn said before it returned is still queued.
                while let Ok(d) = rx.try_recv() {
                    match cancel.guard(relay.pass(&d, sink)).await {
                        Some(true) => {}
                        Some(false) => return TurnEnd::SinkGone,
                        None => return TurnEnd::Canceled,
                    }
                }
                return TurnEnd::Returned(Box::new(Returned {
                    out,
                    acc: relay.acc,
                    streamed: relay.streamed,
                }));
            }
        }
    }
}
