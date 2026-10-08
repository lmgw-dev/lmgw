//! The metered runner's rules: what a turn's row was billed and adds to a total
//! goes into the run's tally, whichever way the turn ended, and which turns
//! can leave the run's cost unknown.

use super::*;

fn request() -> ChatRequest {
    ChatRequest {
        model_alias: "m".into(),
        messages: Vec::new(),
        params: Default::default(),
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    }
}

fn usage(prompt: u64, completion: u64) -> Usage {
    Usage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        ..Default::default()
    }
}

fn answer(ir: &ChatRequest) -> Completion {
    Completion {
        content: vec![crate::ir::ContentPart::text("Hallo.")],
        reasoning: String::new(),
        finish_reason: crate::ir::FinishReason::Stop,
        usage: usage(10, 3),
        model: ir.model_alias.clone(),
        timings: None,
    }
}

/// Bills its row, then fails `canceled` — a turn stopped mid-stream.
struct Stopped;

#[async_trait]
impl TurnRunner for Stopped {
    async fn run_turn(
        &self,
        _ir: &ChatRequest,
        _deadline: Duration,
        sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError> {
        sink.billed(&usage(40, 7));
        sink.billed_cost(RowCost::Priced(470));
        Err(crate::proxy::canceled("stopped by the caller mid-stream"))
    }
}

/// Answers without saying what its row was billed.
struct Silent;

#[async_trait]
impl TurnRunner for Silent {
    async fn run_turn(
        &self,
        ir: &ChatRequest,
        _deadline: Duration,
        _sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError> {
        Ok(answer(ir))
    }
}

/// WP11 server review n4: a turn stopped mid-stream writes its row with the
/// cost so far and fails `canceled`; the run's meter tallies what the row
/// was billed, not nothing. A runner that does not say keeps its
/// completion's usage, and leaves the cost unknown.
#[tokio::test]
async fn the_meter_tallies_what_a_stopped_turn_s_row_was_billed() {
    let mut sink = |_: &crate::ir::StreamDelta| {};
    let stopped = MeteredRunner::new(&Stopped);
    let out = stopped
        .run_turn(&request(), Duration::from_secs(1), &mut sink)
        .await;
    assert!(out.is_err());
    assert_eq!(stopped.tally().usage, usage(40, 7));
    assert_eq!(stopped.tally().cost.micro(), Some(470));

    let silent = MeteredRunner::new(&Silent);
    silent
        .run_turn(&request(), Duration::from_secs(1), &mut sink)
        .await
        .unwrap();
    assert_eq!(silent.tally().usage, usage(10, 3));
    let silent = silent.tally().cost;
    assert!(
        !silent.is_empty() && silent.micro().is_none(),
        "never said: unknown"
    );
}

#[derive(Clone, Copy)]
enum Ends {
    Answered,
    Stopped,
    Failed,
}

/// Says what its row adds to a total as told (`None`: says nothing), then
/// ends as told.
struct Scripted {
    cost: Option<RowCost>,
    ends: Ends,
}

#[async_trait]
impl TurnRunner for Scripted {
    async fn run_turn(
        &self,
        ir: &ChatRequest,
        _deadline: Duration,
        sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError> {
        if let Some(c) = self.cost {
            sink.billed(&Usage::default());
            sink.billed_cost(c);
        }
        match self.ends {
            Ends::Answered => Ok(answer(ir)),
            Ends::Stopped => Err(crate::proxy::canceled("stopped by the caller")),
            Ends::Failed => Err(GatewayError::Upstream {
                status: 502,
                message: "the stream ended mid-way".into(),
                provider_type: None,
            }),
        }
    }
}

/// One turn through a metered runner, then `read` of the runner.
async fn turn<T>(
    cost: Option<RowCost>,
    ends: Ends,
    read: impl FnOnce(&MeteredRunner<'_>) -> T,
) -> T {
    let scripted = Scripted { cost, ends };
    let runner = MeteredRunner::new(&scripted);
    let mut sink = |_: &crate::ir::StreamDelta| {};
    let _ = runner
        .run_turn(&request(), Duration::from_secs(1), &mut sink)
        .await;
    read(&runner)
}

/// Runs the turns in order, each through its own metered runner folded
/// into one meter as a run folds its steps; the meter's cost (`None`: no
/// row added to it).
async fn run(turns: &[(Option<RowCost>, Ends)]) -> Option<Option<i64>> {
    let mut meter = Meter::default();
    for &(cost, ends) in turns {
        meter.absorb(turn(cost, ends, |r| r.tally()).await);
    }
    (!meter.cost.is_empty()).then(|| meter.cost.micro())
}

/// Which turns count: the row's own [`RowCost`] (review WP2b #2, the
/// rollup's rule), whichever way the turn ended — a failure that spent
/// tokens nobody could price leaves the run unknown, as it leaves a gap in
/// Usage. A runner that never said is judged by how the turn ended:
/// answered or stopped leaves the run unknown, a failure adds nothing.
#[tokio::test]
async fn a_turn_adds_what_its_row_adds_to_a_total() {
    use Ends::*;
    use RowCost::*;
    assert_eq!(run(&[(Some(Priced(5)), Answered)]).await, Some(Some(5)));
    assert_eq!(run(&[(Some(Unpriced), Answered)]).await, Some(None));
    assert_eq!(
        run(&[(None, Answered)]).await,
        Some(None),
        "answered and never said"
    );
    assert_eq!(
        run(&[(Some(Priced(5)), Stopped)]).await,
        Some(Some(5)),
        "stopped after the answer"
    );
    assert_eq!(
        run(&[(Some(Unpriced), Stopped)]).await,
        Some(None),
        "stopped before the answer"
    );
    assert_eq!(
        run(&[(Some(Nothing), Failed)]).await,
        None,
        "a refusal that spent nothing: skipped"
    );
    assert_eq!(run(&[(None, Failed)]).await, None, "failed, never said");
    assert_eq!(
        run(&[(Some(Priced(5)), Failed)]).await,
        Some(Some(5)),
        "failed mid-stream after its fee was earned"
    );
    assert_eq!(
        run(&[(Some(Priced(5)), Answered), (Some(Unpriced), Failed)]).await,
        Some(None),
        "a re-routed turn that died mid-stream on an unpriced fallback, \
         having spent tokens: not the other turn's partial sum"
    );
}

/// The run's cost is the sum of its turns' row costs from the first one on,
/// a free local turn a real 0 in it, and unknown for good once any turn's
/// row was unpriced work: never the others' partial sum.
#[tokio::test]
async fn the_run_costs_the_sum_of_its_turns_rows() {
    use Ends::*;
    use RowCost::*;
    let local = (Some(Priced(0)), Answered);
    let cloud = (Some(Priced(5_000)), Answered);
    let unpriced = (Some(Unpriced), Answered);
    let refused = (Some(Nothing), Failed);
    assert_eq!(
        run(&[local, local]).await,
        Some(Some(0)),
        "free, not unknown"
    );
    assert_eq!(
        run(&[local, refused, cloud, cloud]).await,
        Some(Some(10_000))
    );
    assert_eq!(run(&[cloud, unpriced, cloud]).await, Some(None));
    assert_eq!(
        run(&[unpriced, cloud]).await,
        Some(None),
        "unknown first, then priced"
    );
    assert_eq!(run(&[refused]).await, None, "nothing known: no cost at all");
}

/// A turn is a model call once it was answered without an error — as the
/// HTTP run meters count one (review WP2b, second pass) — and carries its
/// row's cost either way: a failed or stopped turn is none.
#[tokio::test]
async fn a_turn_is_a_model_call_only_once_answered() {
    let m = turn(Some(RowCost::Priced(7)), Ends::Answered, |r| r.tally()).await;
    assert_eq!((m.model_calls, m.cost.micro()), (1, Some(7)));

    for ends in [Ends::Failed, Ends::Stopped] {
        let m = turn(Some(RowCost::Priced(7)), ends, |r| r.tally()).await;
        assert_eq!(
            (m.model_calls, m.cost.micro()),
            (0, Some(7)),
            "no model call, and still its row's cost"
        );
        assert_eq!(m.tool_calls, 0);
    }
    let m = turn(None, Ends::Answered, |r| r.tally()).await;
    assert_eq!(
        m.model_calls, 1,
        "answered, whether or not it said its cost"
    );
}
