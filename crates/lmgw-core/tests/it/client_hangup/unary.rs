//! A request that does not stream, whose client goes before the answer: the
//! handler is dropped, and the upstream request with it.

use super::*;

#[tokio::test]
async fn a_structured_answer_still_on_its_way_stops_when_its_client_goes() {
    let (g, gw, model) = local(Says::Nothing).await;
    let asking = tokio::spawn({
        let gw = gw.clone();
        async move {
            gw.client()
                .post(format!("{gw}/v1/chat/completions"))
                .json(&chat_body(false, json_schema()))
                .send()
                .await
        }
    });
    let started = Instant::now();
    while model.live.load(Ordering::SeqCst) == 0 {
        assert!(
            started.elapsed() < HANG_UP_BOUND,
            "the model never got the request"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    asking.abort();
    let gone = Instant::now();
    model_stops(&model, gone).await;
    claim_released(&g).await;
    let row = newest_row(&g.state).await;
    assert_eq!(row.status, 499, "{row:?}");
    assert_eq!(row.error_kind.as_deref(), Some("client_disconnected"));
}
