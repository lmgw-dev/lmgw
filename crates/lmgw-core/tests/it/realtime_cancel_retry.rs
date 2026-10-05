//! A cancel while the gate retries the send bills no prompt (realtime
//! design §11, B2 review 1): the attempt the gate took back ran nothing — a
//! ladder's count said it does not fit and the model climbs, or the
//! container was dead and is being restarted — so a stop during that wait is
//! a `canceled` row without an estimated prompt.

use std::time::Duration;

use serde_json::json;

use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_fakes::{
    events_until, gpu_gateway, next_event, open, send, user_text, Ws,
};

/// The model `model` loaded by the owner, its claim let go: the port its
/// container answers on.
async fn owner_loads(g: &Gpu, model: &str) -> u16 {
    lmgw_core::vram::admit(&g.state, &g.route(model), model)
        .await
        .unwrap()
        .expect("a local model")
        .port()
}

/// A text session on `model`, past its `session.created` and
/// `session.updated`.
async fn text_session_on(addr: &str, model: &str) -> Ws {
    let mut ws = open(addr, &format!("/v1/realtime?model={model}"), &[]).await;
    next_event(&mut ws).await;
    send(
        &mut ws,
        json!({"type": "session.update",
               "session": {"type": "realtime", "output_modalities": ["text"]}}),
    )
    .await;
    next_event(&mut ws).await;
    ws
}

/// Ask for a response, wait until `podman run` has been called `runs` times
/// (the retry's start, held at the run gate), and cancel it there.
async fn cancel_while_starting(g: &Gpu, ws: &mut Ws, runs: usize) {
    send(ws, user_text("Erzähl was")).await;
    events_until(ws, "conversation.item.done").await;
    send(ws, json!({"type": "response.create"})).await;
    events_until(ws, "response.created").await;
    for _ in 0..500 {
        if g.runs().len() == runs {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(g.runs().len(), runs, "the retry's start is held");
    send(ws, json!({"type": "response.cancel"})).await;
    let done = events_until(ws, "response.done").await;
    assert_eq!(done.last().unwrap()["response"]["status"], "cancelled");
}

/// The call's one row: status, error kind and message, prompt tokens.
async fn the_row(g: &Gpu, model: &str) -> (i64, Option<String>, Option<String>, Option<i64>) {
    for _ in 0..500 {
        let row = sqlx::query_as(
            "SELECT status, error_kind, error_msg, prompt_tokens FROM request_logs WHERE \
             requested_alias = ?1 AND ingress_proto = 'realtime'",
        )
        .bind(model)
        .fetch_optional(&g.state.db)
        .await
        .unwrap();
        if let Some(row) = row {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the call wrote no row");
}

fn bills_no_prompt(row: (i64, Option<String>, Option<String>, Option<i64>)) {
    assert_eq!(row.0, 200);
    assert_eq!(row.1.as_deref(), Some("canceled"));
    let msg = row.2.unwrap();
    assert!(!msg.contains("estimated"), "{msg}");
    assert_eq!(row.3, None, "no prompt is being worked on");
}

#[tokio::test]
async fn a_cancel_while_the_ladder_climbs_bills_no_prompt() {
    // The send went out to the base rung beside its count; the count says
    // 100 + 16 tokens do not fit its 64, the attempt is dropped, and the
    // model climbs — its next rung's start held. A stop there bills nothing.
    let g = Gpu::new(24 * GIB, 3, 5).await;
    g.ladder("p", 4 * GIB, &[(8 * GIB, 4096)]).await;
    let addr = gpu_gateway(&g, |_| {}).await;
    owner_loads(&g, "p").await;
    g.world().prompt_tokens = 100;
    let open_runs = g.gate_runs();

    let mut ws = text_session_on(&addr, "p").await;
    cancel_while_starting(&g, &mut ws, 2).await;

    // (The base rung's answer, sent beside the count, was dropped unread.)
    bills_no_prompt(the_row(&g, "p").await);
    open_runs.send(true).unwrap();
}

#[tokio::test]
async fn a_cancel_while_a_dead_container_restarts_bills_no_prompt() {
    // The send finds the container gone (connection refused), and the
    // model is started again — that start held. A stop there bills
    // nothing: a dead container works on no prompt.
    let g = Gpu::new(24 * GIB, 3, 5).await;
    g.model("p", 4 * GIB).await;
    let addr = gpu_gateway(&g, |_| {}).await;
    let port = owner_loads(&g, "p").await;
    g.kill(port).await;
    let open_runs = g.gate_runs();

    let mut ws = text_session_on(&addr, "p").await;
    cancel_while_starting(&g, &mut ws, 2).await;

    bills_no_prompt(the_row(&g, "p").await);
    assert!(
        g.world().chats.is_empty(),
        "no container answered the request"
    );
    open_runs.send(true).unwrap();
}

#[tokio::test]
async fn a_cancel_while_a_dead_ladder_rung_restarts_bills_no_prompt() {
    // The same on a ladder row, whose send runs its own recovery: the count
    // and the send both find the base rung gone.
    let g = Gpu::new(24 * GIB, 3, 5).await;
    g.ladder("p", 4 * GIB, &[(8 * GIB, 4096)]).await;
    let addr = gpu_gateway(&g, |_| {}).await;
    let port = owner_loads(&g, "p").await;
    g.world().prompt_tokens = 10;
    g.kill(port).await;
    let open_runs = g.gate_runs();

    let mut ws = text_session_on(&addr, "p").await;
    cancel_while_starting(&g, &mut ws, 2).await;

    bills_no_prompt(the_row(&g, "p").await);
    assert!(
        g.world().chats.is_empty(),
        "no container answered the request"
    );
    open_runs.send(true).unwrap();
}
