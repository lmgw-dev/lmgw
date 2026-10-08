//! A classify stage that aborts (§2.4) still hands back what its calls
//! spent (review WP2b #1): a failed call now carries its row's known cost —
//! an Anthropic 502 after `message_start` has its prompt tokens priced — so a
//! failed run's cost and tokens must not read lower than its rows.

use super::*;
use crate::pricing::RowCost;

/// Bills every call's row — tokens and a known cost — then fails it 5xx, as
/// a stream that died after its answer began does.
struct BillsThenFails {
    calls: AtomicUsize,
}

#[async_trait]
impl TurnRunner for BillsThenFails {
    async fn run_turn(
        &self,
        _ir: &ChatRequest,
        _deadline: Duration,
        sink: &mut dyn DeltaSink,
    ) -> Result<Completion, GatewayError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        sink.billed(&Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(0),
            ..Default::default()
        });
        sink.billed_cost(RowCost::Priced(100));
        Err(upstream_down())
    }
}

#[tokio::test]
async fn an_aborted_classify_stage_keeps_what_its_calls_spent() {
    let h = Harness::new().await;
    let agent = agent_from(&doc(), json!({}));
    let exec = FakeExec::with(vec![
        (
            "gws__search",
            json_result(
                json!({ "messages": (0..8).map(|i| json!({"id": format!("m{i}")})).collect::<Vec<_>>() }),
            ),
        ),
        ("gws__get", json_result(json!({ "subject": "s" }))),
    ]);
    let runner = BillsThenFails {
        calls: AtomicUsize::new(0),
    };
    let mut run = h.run(&agent, Some(&runner), &exec);
    let e = run.classify_run(Phase::Classify).await.unwrap_err();
    assert!(e.starts_with("model unavailable"), "{e}");
    let calls = runner.calls.load(Ordering::SeqCst) as i64;
    assert_eq!(calls, 3, "it aborted after the third failure");

    // The failed run's result: every failed call's row, none dropped.
    let result = run
        .failure_result(Phase::Classify)
        .expect("a run that made calls has a result");
    assert_eq!(result["cost_micro"], json!(100 * calls), "{result}");
    assert_eq!(result["usage"]["prompt_tokens"], json!(10 * calls));
    assert_eq!(result["model_calls"], json!(0), "none was answered");
    assert_eq!(result["tool_calls"], json!(9), "the source and every fetch");
}
