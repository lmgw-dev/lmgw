//! A synthesized `/v1/responses` run's turns hand on as the run's request
//! (openai egress design §14.4, review B1): its route check, its origin and
//! its chain. A guest's run (`x-lmgw-background: on`) hands a turn on as a
//! guest, so a local fallback is admitted only where it disturbs nobody —
//! never by evicting the owner's model.

use super::*;

#[tokio::test]
async fn a_background_runs_turn_hands_on_as_a_guest() {
    let g = Gpu::new(20 * GIB, 3, 2).await;
    let gw = common::serve(g.state.clone()).await;
    g.model("mine", 12 * GIB).await;
    g.model("p", 10 * GIB).await;
    // The owner's model, loaded and idle: an owner's request would evict it
    // for `p`, a guest's never does.
    assert_eq!(ask(&gw, "mine").await.status(), 200);
    assert_eq!(g.runs(), ["mine"]);

    // A compatible row nothing answers on: the run's first turn finds it
    // unreachable at the send, and hands on to `p`.
    let (port, _held) = crate::common::refusing_port();
    upstream_alias(
        &g,
        "a",
        &format!("http://127.0.0.1:{port}/v1"),
        Protocol::OpenaiCompatible,
    )
    .await;
    set_fallback(&g.state, "models", "a", Some("p")).await;

    let resp = gw
        .client()
        .post(format!("{gw}/v1/responses"))
        .header("x-lmgw-background", "on")
        .json(&json!({"model": "a", "input": "hi"}))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_ne!(status, 200, "{body}");
    assert!(
        body.contains("'p' deferred") && body.contains("(tried: a → p)"),
        "{status} {body}"
    );
    assert!(
        g.stops().is_empty(),
        "the owner's model stays: {:?}",
        g.stops()
    );
    assert_eq!(g.runs(), ["mine"], "nothing started for the guest");
}
