//! The owner's streamed `/v1/responses` run: its loop runs in a task of its
//! own, so before 2026-10-24 it heard its client's going only at the next
//! frame it failed to send, and a model that wrote the client nothing (a
//! long prefill, frames that carry nothing) went on generating for as long
//! as it liked. Now the hang-up stops the model call at once, and its row
//! says `canceled` (per-request background design §11.16, owner decision
//! 2026-10-24). A tool call in flight at the hang-up finishes
//! (`background_header::runs`).

use futures::StreamExt;

use super::*;

/// Open the owner's streamed run on `m`, read its first frame
/// (`response.created`), wait until `model` is asked, and go.
async fn open_run_and_leave(gw: &Gw, model: &Model) -> Instant {
    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .json(&json!({"model": "m", "input": "write the story's outline", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.expect("the stream opens").unwrap();
    assert!(!first.is_empty());
    let asked = Instant::now();
    while model.live.load(Ordering::SeqCst) == 0 {
        assert!(asked.elapsed() < HANG_UP_BOUND, "the model was never asked");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(stream);
    Instant::now()
}

/// What follows the client's going: the model's connection closes, the
/// claim goes, the model call's row says `canceled`.
async fn ends(g: &Gpu, model: &Model, gone: Instant) {
    model_stops(model, gone).await;
    claim_released(g).await;
    let row = newest_row(&g.state).await;
    assert_eq!(row.error_kind.as_deref(), Some("canceled"), "{row:?}");
}

#[tokio::test]
async fn the_owner_s_run_whose_model_still_prefills_stops_when_its_client_goes() {
    let (g, gw, model) = local(Says::Silence).await;
    let gone = open_run_and_leave(&gw, &model).await;
    ends(&g, &model, gone).await;
}

#[tokio::test]
async fn the_owner_s_run_that_writes_the_client_nothing_stops_when_it_goes() {
    let (g, gw, model) = local(Says::Nothing).await;
    let gone = open_run_and_leave(&gw, &model).await;
    ends(&g, &model, gone).await;
}
