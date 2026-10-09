//! The loop relays a turn as it arrives (chat-voice design §7.2), against a
//! scripted runner that streams step by step and can wait between steps, and
//! takes the turn's stop as the in-process model calls do.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::Notify;

use super::*;

/// One step of a scripted turn.
enum Step {
    Delta(StreamDelta),
    /// Wait until the sink has seen this many `Text` events.
    AwaitTexts(usize),
    /// Never go on by itself: a model still generating, until the turn is
    /// stopped.
    Hang,
}

/// What the sink has seen, shared with the runner.
#[derive(Default)]
struct Seen {
    events: Mutex<Vec<LoopEvent>>,
    texts: Mutex<usize>,
    changed: Notify,
}

impl Seen {
    async fn texts_at_least(&self, n: usize) {
        loop {
            let notified = self.changed.notified();
            if *self.texts.lock().unwrap() >= n {
                return;
            }
            notified.await;
        }
    }
}

struct SeenSink {
    seen: Arc<Seen>,
    /// Give up at the first `Text`.
    quit_at_text: bool,
    /// Raise the run's cancel at the first `Text`.
    cancel_at_text: Option<crate::proxy::StopHandle>,
}

#[async_trait]
impl EventSink for SeenSink {
    async fn emit(&mut self, ev: LoopEvent) -> bool {
        let text = matches!(ev, LoopEvent::Text(_));
        self.seen.events.lock().unwrap().push(ev);
        if text {
            *self.seen.texts.lock().unwrap() += 1;
            self.seen.changed.notify_waiters();
            if let Some(cancel) = &self.cancel_at_text {
                cancel.stop();
            }
            return !self.quit_at_text;
        }
        true
    }
}

/// Plays one script per turn; says when a turn's future was dropped, and
/// whether the turn saw its stop raised before it returned.
struct StepRunner {
    turns: Mutex<Vec<Vec<Step>>>,
    seen: Arc<Seen>,
    dropped: Arc<AtomicBool>,
    /// Take the turn's stop, as `proxy::stream_once_on` does. Off: a runner
    /// that never takes it, which the loop drops.
    takes_stop: bool,
    saw_stop: Arc<AtomicBool>,
}

/// Raises its flag when dropped — with the turn future that owns it.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl TurnRunner for StepRunner {
    async fn run_turn(
        &self,
        _ir: &ChatRequest,
        _deadline: Duration,
        sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError> {
        let steps = {
            let mut turns = self.turns.lock().unwrap();
            if turns.is_empty() {
                return Err(GatewayError::Internal("the script is over".into()));
            }
            turns.remove(0)
        };
        let _flag = DropFlag(self.dropped.clone());
        let stop = if self.takes_stop { sink.stop() } else { None };
        let stopped = || async {
            match &stop {
                Some(s) => s.raised().await,
                None => std::future::pending().await,
            }
        };
        for step in steps {
            match step {
                Step::Delta(d) => sink.on_delta(&d),
                Step::AwaitTexts(n) => self.seen.texts_at_least(n).await,
                Step::Hang => stopped().await,
            }
            if stop
                .as_ref()
                .is_some_and(crate::proxy::StopSignal::is_raised)
            {
                self.saw_stop.store(true, Ordering::SeqCst);
                return Err(crate::proxy::canceled("stopped by the caller mid-stream"));
            }
        }
        // A streaming runner's completion only names the model; the loop
        // folds the turn from its deltas.
        Ok(Completion {
            content: Vec::new(),
            reasoning: String::new(),
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
            model: "m".into(),
            timings: None,
        })
    }
}

struct EchoExec;

#[async_trait]
impl ToolExecutor for EchoExec {
    async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        ToolOutcome::ok(ToolResultBlock::one(format!("{name}:{args}")))
    }
}

fn text(t: &str) -> Step {
    Step::Delta(StreamDelta::TextDelta(t.into()))
}

fn start(index: usize, id: &str) -> Step {
    Step::Delta(StreamDelta::ToolCallStart {
        index,
        id: id.into(),
        name: "search".into(),
    })
}

fn args(index: usize, fragment: &str) -> Step {
    Step::Delta(StreamDelta::ToolCallArgsDelta {
        index,
        fragment: fragment.into(),
    })
}

fn request() -> ChatRequest {
    ChatRequest {
        model_alias: "alias".into(),
        messages: vec![Message::text(Role::User, "go")],
        params: Default::default(),
        tools: Vec::new(),
        tool_choice: None,
        stream: false,
        passthrough: Default::default(),
        llama_kwargs_enabled: None,
        anthropic_beta: Vec::new(),
    }
}

fn config() -> RunConfig {
    RunConfig::new(
        vec![ResolvedTool::server_side(
            "web",
            ToolDef {
                name: "search".into(),
                description: None,
                parameters: json!({"type": "object", "properties": {}}),
            },
        )],
        Budget {
            max_tool_calls: 10,
            wall_clock: Duration::from_secs(60),
        },
        true,
    )
}

fn step_runner(turns: Vec<Vec<Step>>, seen: &Arc<Seen>) -> StepRunner {
    StepRunner {
        turns: Mutex::new(turns),
        seen: seen.clone(),
        dropped: Arc::new(AtomicBool::new(false)),
        takes_stop: true,
        saw_stop: Arc::new(AtomicBool::new(false)),
    }
}

/// How a scripted run ended: its result, what the sink saw, and the runner
/// (whose flags say how its last turn ended).
struct Ran {
    res: RunResult,
    events: Vec<LoopEvent>,
    runner: StepRunner,
}

/// Run `runner` to the end, or fail after 10 s — which is what a loop that
/// waits for the turn to end before relaying it does to a runner that waits
/// for the relay.
async fn run_with(runner: StepRunner, mut sink: SeenSink, cfg: RunConfig) -> Ran {
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        run(request(), cfg, &runner, &EchoExec, &mut sink),
    )
    .await
    .expect("the run is not waiting for a turn to end before relaying it")
    .unwrap();
    let events = std::mem::take(&mut *sink.seen.events.lock().unwrap());
    Ran {
        res,
        events,
        runner,
    }
}

async fn run_script(
    turns: Vec<Vec<Step>>,
    quit_at_text: bool,
) -> (RunResult, Vec<LoopEvent>, Arc<AtomicBool>) {
    let seen = Arc::new(Seen::default());
    let runner = step_runner(turns, &seen);
    let sink = SeenSink {
        seen,
        quit_at_text,
        cancel_at_text: None,
    };
    let ran = run_with(runner, sink, config()).await;
    (ran.res, ran.events, ran.runner.dropped)
}

#[tokio::test]
async fn a_slow_turn_yields_its_text_before_it_ends() {
    // The runner goes on only once the sink saw its first token.
    let (res, events, _) = run_script(
        vec![vec![text("Hel"), Step::AwaitTexts(1), text("lo")]],
        false,
    )
    .await;
    assert_eq!(res.reason, StopReason::Done(FinishReason::Stop));
    let texts: Vec<&LoopEvent> = events
        .iter()
        .filter(|e| matches!(e, LoopEvent::Text(_)))
        .collect();
    assert_eq!(
        texts,
        [
            &LoopEvent::Text("Hel".into()),
            &LoopEvent::Text("lo".into())
        ]
    );
    let last = res.messages.last().unwrap();
    assert_eq!(last.content, vec![ContentPart::text("Hello")]);
}

#[tokio::test]
async fn arrival_order_is_kept_across_text_reasoning_and_calls() {
    let (res, events, _) = run_script(
        vec![
            vec![
                Step::Delta(StreamDelta::ReasoningDelta("think".into())),
                text("Let me "),
                start(0, "c1"),
                Step::AwaitTexts(1),
                text("check "),
                args(0, "{\"q\":"),
                start(1, "c2"),
                args(0, "1}"),
                args(1, "{\"q\":2}"),
                Step::Delta(StreamDelta::Stop(FinishReason::ToolUse)),
            ],
            vec![start(5, "c3"), args(5, "{}")],
            vec![text("done")],
        ],
        false,
    )
    .await;
    assert_eq!(res.reason, StopReason::Done(FinishReason::Stop));
    let streamed: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::Text(t) => Some(format!("text {t}")),
            LoopEvent::Reasoning(t) => Some(format!("reasoning {t}")),
            LoopEvent::CallStarted { index, call_id, .. } => {
                Some(format!("start {index} {call_id}"))
            }
            LoopEvent::CallArgs { index, fragment } => Some(format!("args {index} {fragment}")),
            LoopEvent::CallResult { index, .. } => Some(format!("result {index}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        streamed,
        [
            "reasoning think",
            "text Let me ",
            "start 0 c1",
            "text check ",
            "args 0 {\"q\":",
            "start 1 c2",
            "args 0 1}",
            "args 1 {\"q\":2}",
            "result 0",
            "result 1",
            // Run-global: the second turn's first call is the third.
            "start 2 c3",
            "args 2 {}",
            "result 2",
            "text done",
        ]
    );
    // The calls were folded whole, arguments and all.
    assert!(events.iter().any(|e| matches!(
        e,
        LoopEvent::CallReady { index: 0, args, .. } if args == &json!({"q": 1})
    )));
}

/// A client gone mid-turn stops the turn and waits for it to end, so its
/// model call writes its row (WP2 review M1), and nothing more is relayed.
#[tokio::test]
async fn a_sink_that_gives_up_mid_turn_stops_the_turn_and_awaits_it() {
    let seen = Arc::new(Seen::default());
    let runner = step_runner(vec![vec![text("a"), Step::Hang]], &seen);
    let sink = SeenSink {
        seen,
        quit_at_text: true,
        cancel_at_text: None,
    };
    let ran = run_with(runner, sink, config()).await;
    assert!(ran.res.aborted);
    assert_eq!(
        ran.res.reason,
        StopReason::Incomplete("client_disconnected")
    );
    assert!(
        ran.runner.saw_stop.load(Ordering::SeqCst),
        "the turn saw its stop raised and returned by itself"
    );
    assert_eq!(
        ran.events.last(),
        Some(&LoopEvent::Text("a".into())),
        "nothing after the event the sink refused"
    );
}

/// A runner that never took the stop (a unary one) cannot be stopped, so
/// the loop drops it, which closes its upstream request.
#[tokio::test]
async fn a_turn_that_never_took_the_stop_is_dropped() {
    let seen = Arc::new(Seen::default());
    let mut runner = step_runner(vec![vec![text("a"), Step::Hang]], &seen);
    runner.takes_stop = false;
    let sink = SeenSink {
        seen,
        quit_at_text: true,
        cancel_at_text: None,
    };
    let ran = run_with(runner, sink, config()).await;
    assert!(ran.res.aborted);
    assert!(ran.runner.dropped.load(Ordering::SeqCst));
    assert!(!ran.runner.saw_stop.load(Ordering::SeqCst));
}

/// A cancel mid-turn stops the turn and awaits it: no partial turn in the
/// record, and what the turn queued after the cancel is not relayed.
#[tokio::test]
async fn a_cancel_mid_turn_stops_the_turn_and_relays_nothing_after_it() {
    let seen = Arc::new(Seen::default());
    // Three tokens queued at once; the cancel is raised as the first is
    // relayed, so the other two are still queued when it lands.
    let runner = step_runner(
        vec![vec![text("a"), text("b"), text("c"), Step::Hang]],
        &seen,
    );
    let (handle, signal) = crate::proxy::stop_pair();
    let sink = SeenSink {
        seen,
        quit_at_text: false,
        cancel_at_text: Some(handle),
    };
    let ran = run_with(runner, sink, config().with_cancel(Cancel::signal(signal))).await;
    assert_eq!(ran.res.reason, StopReason::Incomplete(REASON_CANCELED));
    assert!(ran.runner.saw_stop.load(Ordering::SeqCst));
    let texts: Vec<&LoopEvent> = ran
        .events
        .iter()
        .filter(|e| matches!(e, LoopEvent::Text(_)))
        .collect();
    assert_eq!(texts, [&LoopEvent::Text("a".into())]);
    assert_eq!(
        ran.res.messages,
        vec![Message::text(Role::User, "go")],
        "no partial turn in the record"
    );
}

/// A cancel lands while an emit waits on a reader that stopped reading: the
/// run ends anyway, and does not wait for the reader.
#[tokio::test]
async fn a_cancel_lands_while_an_emit_waits_on_a_stalled_reader() {
    /// Takes the first `Text` and never returns from it.
    struct Stalled(Arc<Notify>);
    #[async_trait]
    impl EventSink for Stalled {
        async fn emit(&mut self, ev: LoopEvent) -> bool {
            if matches!(ev, LoopEvent::Text(_)) {
                self.0.notify_one();
                std::future::pending::<()>().await;
            }
            true
        }
    }
    let seen = Arc::new(Seen::default());
    let runner = step_runner(vec![vec![text("a"), Step::Hang]], &seen);
    let entered = Arc::new(Notify::new());
    let mut sink = Stalled(entered.clone());
    let (handle, signal) = crate::proxy::stop_pair();
    let cfg = config().with_cancel(Cancel::signal(signal));
    let (res, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run(request(), cfg, &runner, &EchoExec, &mut sink), async {
            entered.notified().await;
            handle.stop();
        })
    })
    .await
    .expect("the cancel does not wait for the reader");
    assert_eq!(res.unwrap().reason, StopReason::Incomplete(REASON_CANCELED));
    assert!(runner.saw_stop.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_turn_failing_mid_stream_has_already_shown_its_tokens() {
    struct Failing;
    #[async_trait]
    impl TurnRunner for Failing {
        async fn run_turn(
            &self,
            _ir: &ChatRequest,
            _deadline: Duration,
            sink: &mut dyn DeltaSink,
        ) -> Result<Completion, GatewayError> {
            sink.on_delta(&StreamDelta::TextDelta("partial".into()));
            Err(GatewayError::Internal("the stream broke".into()))
        }
    }
    let mut sink = CollectSink::default();
    let res = run(request(), config(), &Failing, &EchoExec, &mut sink).await;
    assert!(res.is_err());
    assert!(sink.0.contains(&LoopEvent::Text("partial".into())));
}

/// A turn that fails after tools ran: the error keeps the record of the
/// turns before it, so the model can learn its tools ran (WP2 review M3).
#[tokio::test]
async fn a_turn_failing_after_tools_ran_keeps_their_record_in_the_error() {
    let seen = Arc::new(Seen::default());
    // Turn 1 makes two calls; turn 2 finds the script over and fails.
    let runner = step_runner(vec![two_calls()], &seen);
    let mut sink = CollectSink::default();
    let Err(e) = run(request(), config(), &runner, &EchoExec, &mut sink).await else {
        panic!("turn 2 fails");
    };
    assert!(e.to_string().contains("the script is over"), "{e}");
    let roles: Vec<Role> = e.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::Tool]);
    assert!(matches!(
        e.messages[1].content.as_slice(),
        [ContentPart::ToolUse { .. }, ContentPart::ToolUse { .. }]
    ));
    let results = last_results(&RunResult {
        reason: StopReason::Done(FinishReason::Stop),
        usage: e.usage,
        messages: e.messages,
        pending: Vec::new(),
        aborted: false,
    });
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|(_, _, error)| !error));
}

// ---------------------------------------------------------------------------
// An abandoned call gets its result (§7.3)
// ---------------------------------------------------------------------------

/// A tool that never answers, and raises `flag` once it was called: the
/// call was made when the cancel landed.
struct Forever(Option<Arc<AtomicBool>>);

#[async_trait]
impl ToolExecutor for Forever {
    async fn call(&self, _name: &str, _args: &Value) -> ToolOutcome {
        if let Some(flag) = &self.0 {
            flag.store(true, Ordering::SeqCst);
        }
        std::future::pending().await
    }
}

/// Records events; raises `flag` at the first `CallReady` when
/// `raise_at_ready` (before any call was made), and gives up at the first
/// `CallResult` when `quit_at_result`.
struct RaiseSink {
    flag: Arc<AtomicBool>,
    events: Vec<LoopEvent>,
    raise_at_ready: bool,
    quit_at_result: bool,
}

#[async_trait]
impl EventSink for RaiseSink {
    async fn emit(&mut self, ev: LoopEvent) -> bool {
        let ready = matches!(ev, LoopEvent::CallReady { .. });
        let result = matches!(ev, LoopEvent::CallResult { .. });
        self.events.push(ev);
        if ready && self.raise_at_ready {
            self.flag.store(true, Ordering::SeqCst);
        }
        !(result && self.quit_at_result)
    }
}

/// `CallResult`s as `(index, result text)`.
fn reported(events: &[LoopEvent]) -> Vec<(usize, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::CallResult { index, blocks, .. } => {
                Some((*index, crate::ir::flatten_tool_result(blocks).0))
            }
            _ => None,
        })
        .collect()
}

/// The calls of the record's last tool message: `(id, result text, error)`.
fn last_results(res: &RunResult) -> Vec<(String, String, bool)> {
    let last = res.messages.last().unwrap();
    assert_eq!(last.role, Role::Tool, "{:?}", res.messages);
    last.content
        .iter()
        .map(|p| match p {
            ContentPart::ToolResult {
                id,
                content,
                is_error,
                ..
            } => (
                id.clone(),
                crate::ir::flatten_tool_result(content).0,
                *is_error,
            ),
            other => panic!("not a result: {other:?}"),
        })
        .collect()
}

fn two_calls() -> Vec<Step> {
    vec![start(0, "c1"), args(0, "{}"), start(1, "c2"), args(1, "{}")]
}

#[tokio::test]
async fn a_cancel_mid_tool_leaves_a_record_in_which_every_call_has_its_result() {
    let flag = Arc::new(AtomicBool::new(false));
    let runner = step_runner(vec![two_calls()], &Arc::new(Seen::default()));
    let mut sink = RaiseSink {
        flag: flag.clone(),
        events: Vec::new(),
        raise_at_ready: false,
        quit_at_result: false,
    };
    let cfg = config().with_cancel(Cancel::flag(flag.clone()));
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        run(request(), cfg, &runner, &Forever(Some(flag)), &mut sink),
    )
    .await
    .expect("the cancel lands within a poll")
    .unwrap();
    assert_eq!(res.reason, StopReason::Incomplete(REASON_CANCELED));
    let n = res.messages.len();
    assert!(matches!(
        res.messages[n - 2].content.as_slice(),
        [ContentPart::ToolUse { .. }, ContentPart::ToolUse { .. }]
    ));
    assert_eq!(
        last_results(&res),
        [
            ("c1".to_string(), ABANDONED_CALL.to_string(), true),
            ("c2".to_string(), ABANDONED_CALL.to_string(), true),
        ]
    );
    // …and reported as such, before the terminal event.
    assert_eq!(
        reported(&sink.events),
        [
            (0, ABANDONED_CALL.to_string()),
            (1, ABANDONED_CALL.to_string())
        ]
    );
}

/// A cancel already raised when the calls would start made none of them,
/// and their results say so rather than that they may have run.
#[tokio::test]
async fn a_cancel_before_the_calls_start_records_them_as_unmade() {
    let flag = Arc::new(AtomicBool::new(false));
    let runner = step_runner(vec![two_calls()], &Arc::new(Seen::default()));
    let mut sink = RaiseSink {
        flag: flag.clone(),
        events: Vec::new(),
        raise_at_ready: true,
        quit_at_result: false,
    };
    let cfg = config().with_cancel(Cancel::flag(flag));
    let called = Arc::new(AtomicBool::new(false));
    let res = run(
        request(),
        cfg,
        &runner,
        &Forever(Some(called.clone())),
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(res.reason, StopReason::Incomplete(REASON_CANCELED));
    assert!(!called.load(Ordering::SeqCst), "no call was made");
    assert_eq!(
        last_results(&res),
        [
            ("c1".to_string(), UNMADE_CALL.to_string(), true),
            ("c2".to_string(), UNMADE_CALL.to_string(), true),
        ]
    );
    assert_eq!(
        reported(&sink.events),
        [(0, UNMADE_CALL.to_string()), (1, UNMADE_CALL.to_string())]
    );
}

fn decided(id: &str, approved: bool) -> DecidedCall {
    DecidedCall {
        call: PendingCall {
            approval_id: id.into(),
            call_id: id.into(),
            name: "search".into(),
            args: json!({}),
            server_label: "web".into(),
            needs_approval: true,
        },
        approved,
        denial: "the owner said no".into(),
        by: None,
    }
}

/// A cancel while a resume's approved calls run keeps each denial, and
/// reports one result per resume entry at the index its `CallReady` had.
#[tokio::test]
async fn a_cancel_while_settling_a_resume_keeps_each_denial() {
    let flag = Arc::new(AtomicBool::new(false));
    let runner = step_runner(Vec::new(), &Arc::new(Seen::default()));
    let mut sink = RaiseSink {
        flag: flag.clone(),
        events: Vec::new(),
        raise_at_ready: false,
        quit_at_result: false,
    };
    let mut cfg = config().with_cancel(Cancel::flag(flag.clone()));
    cfg.resume = vec![decided("c1", false), decided("c2", true)];
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        run(request(), cfg, &runner, &Forever(Some(flag)), &mut sink),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(res.reason, StopReason::Incomplete(REASON_CANCELED));
    assert_eq!(
        last_results(&res),
        [
            ("c1".to_string(), "the owner said no".to_string(), true),
            ("c2".to_string(), ABANDONED_CALL.to_string(), true),
        ]
    );
    assert_eq!(
        reported(&sink.events),
        [
            (0, "the owner said no".to_string()),
            (1, ABANDONED_CALL.to_string())
        ]
    );
}

#[tokio::test]
async fn a_client_gone_while_results_are_reported_keeps_them_in_the_record() {
    let runner = step_runner(vec![two_calls()], &Arc::new(Seen::default()));
    let mut sink = RaiseSink {
        flag: Arc::new(AtomicBool::new(false)),
        events: Vec::new(),
        raise_at_ready: false,
        quit_at_result: true,
    };
    let res = run(request(), config(), &runner, &EchoExec, &mut sink)
        .await
        .unwrap();
    assert!(res.aborted);
    let results = last_results(&res);
    assert_eq!(results.len(), 2, "both calls ran and both are recorded");
    assert!(results.iter().all(|(_, _, error)| !error));
}
