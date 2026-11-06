//! A Chat turn that draws (design §6): a scripted chat model calls
//! `image__generate`, the picture is stored as the thread's attachment and
//! bound to the reply, the record keeps its ref, and every later model call
//! — of the same turn and of the next — gets the picture back.

use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::SharedState;
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{gateway, image_row, job, mount_sd, Z_IMAGE};
use crate::common::Gw;

/// A chat alias `my-model` onto `chat`, which answers `bodies` in order
/// (the last one repeats).
pub(super) async fn chat_model(state: &SharedState, chat: &MockServer, bodies: &[String]) {
    let answers = bodies
        .iter()
        .map(|b| {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(b.clone(), "text/event-stream")
        })
        .collect();
    chat_model_answers(state, chat, answers).await;
}

/// [`chat_model`], its answers as given (a delay among them).
pub(super) async fn chat_model_answers(
    state: &SharedState,
    chat: &MockServer,
    answers: Vec<ResponseTemplate>,
) {
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "chat-up".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: chat.uri(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "my-model".into(),
            upstream_id: up,
            upstream_model_id: "tgt-model".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    let last = answers.len() - 1;
    for (i, answer) in answers.into_iter().enumerate() {
        let m = Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(answer)
            .with_priority((i + 1) as u8);
        let m = if i == last { m } else { m.up_to_n_times(1) };
        m.mount(chat).await;
    }
}

/// A streamed tool call of `name` with `args`.
pub(super) fn tool_call(id: &str, name: &str, args: Value) -> String {
    let args = serde_json::to_string(&args.to_string()).unwrap();
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\
         \"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{args}}}}}]}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

pub(super) fn answer(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

/// A thread on `my-model` with the `image` toolset attached.
pub(super) async fn thread(gw: &Gw) -> i64 {
    let t: Value = gw
        .client()
        .post(format!("{gw}/chat/api/threads"))
        .json(&json!({"model_alias": "my-model"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = t["id"].as_i64().unwrap();
    gw.client()
        .post(format!("{gw}/chat/api/threads/{tid}/settings"))
        .json(&json!({"mcp_tools": [{"server_label": "image"}]}))
        .send()
        .await
        .unwrap();
    tid
}

pub(super) async fn send(gw: &Gw, tid: i64, text: &str) -> String {
    gw.client()
        .post(format!("{gw}/chat/api/threads/{tid}/send"))
        .json(&json!({"content": text}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

pub(super) async fn detail(gw: &Gw, tid: i64) -> Value {
    gw.client()
        .get(format!("{gw}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// The chat model's request bodies, in order.
async fn chat_bodies(chat: &MockServer) -> Vec<String> {
    chat.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/chat/completions")
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .collect()
}

#[tokio::test]
async fn a_drawn_picture_is_stored_bound_to_the_reply_and_replayed() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let chat = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    chat_model(
        &state,
        &chat,
        &[
            tool_call(
                "c1",
                "image__generate",
                json!({"model": "image/z", "prompt": "a red square"}),
            ),
            answer("here it is"),
        ],
    )
    .await;
    let tid = thread(&gw).await;

    let body = send(&gw, tid, "draw me a red square").await;
    assert!(body.contains("image__generate"), "{body}");
    assert!(body.contains("here it is"), "{body}");

    let d = detail(&gw, tid).await;
    let reply = &d["messages"][1];
    let atts = reply["attachments"].as_array().expect("attachments");
    assert_eq!(atts.len(), 1, "{reply:#}");
    let att = &atts[0];
    assert_eq!(att["source"], json!("tool"));
    assert_eq!(att["call_id"], json!("c1"));
    assert_eq!(att["kind"], json!("image"));
    assert_eq!(att["meta"]["media"]["model"], json!("image/z"));
    assert_eq!(att["meta"]["media"]["tool"], json!("image__generate"));
    assert!(att["name"].as_str().unwrap().starts_with("image-z-"));
    let id = att["id"].as_i64().unwrap();
    assert!(d["draft_attachments"].as_array().unwrap().is_empty());

    // The record keeps the ref, never the bytes.
    let ir = reply["ir_messages"].as_str().expect("a tool record");
    assert!(ir.contains(&format!("lmgw:attachment/{id}")), "{ir}");
    assert!(ir.contains(&format!("att:{id}")), "{ir}");
    assert!(!ir.contains(&super::b64(&super::png())), "{ir}");

    // The turn's second model call got the picture back: on this
    // text-only tool slot as the egress's placeholder for an image.
    let bodies = chat_bodies(&chat).await;
    assert_eq!(bodies.len(), 2);
    assert!(bodies[1].contains("image/png image,"), "{}", bodies[1]);
    assert!(!bodies[1].contains("lmgw:attachment"), "{}", bodies[1]);

    // And the next turn's first one too.
    let _ = send(&gw, tid, "nice").await;
    let bodies = chat_bodies(&chat).await;
    assert_eq!(bodies.len(), 3);
    assert!(bodies[2].contains("image/png image,"), "{}", bodies[2]);

    // The model row is the Chat's.
    let rows = super::rows_of(&state, "image", 1).await;
    assert_eq!(rows[0]["proto"], json!("chat"));
}

#[tokio::test]
async fn an_edit_of_another_threads_picture_is_refused() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let chat = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, true)]).await;
    // A picture in another thread.
    let other = thread(&gw).await;
    let foreign = store::insert_chat_attachment_new(
        &state.db,
        other,
        &store::NewAttachment::plain("image", "cat.png", "image/png", &super::png()),
    )
    .await
    .unwrap();
    chat_model(
        &state,
        &chat,
        &[
            tool_call(
                "c1",
                "image__edit",
                json!({"model": "image/z", "prompt": "blue", "image": format!("att:{foreign}")}),
            ),
            answer("could not"),
        ],
    )
    .await;
    let tid = thread(&gw).await;
    let _ = send(&gw, tid, "edit that cat").await;

    let bodies = chat_bodies(&chat).await;
    assert!(
        bodies[1].contains(&format!("there is no att:{foreign} in this conversation")),
        "{}",
        bodies[1]
    );
    assert!(super::submitted(&sd).await.is_empty(), "nothing was drawn");
    let d = detail(&gw, tid).await;
    assert!(d["messages"][1]["attachments"]
        .as_array()
        .unwrap()
        .is_empty());
}

/// Keep (review finding 2): a temporary chat's picture gets a new id when
/// the thread is written, and the record's refs follow it — the next turn's
/// model call gets the picture back, not `[picture att:N was deleted]`.
#[tokio::test]
async fn a_kept_chat_s_record_names_its_pictures_by_their_new_ids() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let chat = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    chat_model(
        &state,
        &chat,
        &[
            tool_call(
                "c1",
                "image__generate",
                json!({"model": "image/z", "prompt": "a red square"}),
            ),
            answer("here it is"),
        ],
    )
    .await;
    let t: Value = gw
        .client()
        .post(format!("{gw}/chat/api/threads"))
        .json(&json!({"model_alias": "my-model", "temporary": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let temp = t["id"].as_i64().unwrap();
    gw.client()
        .post(format!("{gw}/chat/api/threads/{temp}/settings"))
        .json(&json!({"mcp_tools": [{"server_label": "image"}]}))
        .send()
        .await
        .unwrap();
    let body = send(&gw, temp, "draw me a red square").await;
    assert!(body.contains("here it is"), "{body}");
    let before = detail(&gw, temp).await;
    let temp_att = before["messages"][1]["attachments"][0]["id"]
        .as_i64()
        .unwrap();
    assert!(temp_att < 0, "{before:#}");

    let kept: Value = gw
        .client()
        .post(format!("{gw}/chat/api/threads/{temp}/persist"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tid = kept["id"].as_i64().unwrap();
    let d = detail(&gw, tid).await;
    let reply = &d["messages"][1];
    let id = reply["attachments"][0]["id"].as_i64().unwrap();
    assert!(id > 0, "{reply:#}");
    let ir = reply["ir_messages"].as_str().unwrap();
    assert!(ir.contains(&format!("lmgw:attachment/{id}")), "{ir}");
    assert!(ir.contains(&format!("att:{id}")), "{ir}");
    assert!(!ir.contains(&format!("att:{temp_att}")), "{ir}");

    // The kept thread's next turn gets the picture back.
    let _ = send(&gw, tid, "nice").await;
    let bodies = chat_bodies(&chat).await;
    let last = bodies.last().unwrap();
    assert!(last.contains("image/png image,"), "{last}");
    assert!(!last.contains("was deleted"), "{last}");
}

/// `(status, error_kind)` of every `request_logs` row of class `class`, or
/// of `ingress_proto` `proto`, once there are `n` (rows are written by
/// their own task).
pub(super) async fn rows_where(
    state: &SharedState,
    (class, proto): (Option<&str>, Option<&str>),
    n: usize,
) -> Vec<(i64, Option<String>)> {
    for _ in 0..500 {
        let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
            "SELECT status, error_kind FROM request_logs
             WHERE (?1 IS NULL OR class = ?1) AND (?2 IS NULL OR ingress_proto = ?2)
             ORDER BY id",
        )
        .bind(class)
        .bind(proto)
        .fetch_all(&state.db)
        .await
        .unwrap();
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("fewer than {n} rows of {class:?} / {proto:?}");
}

/// The thread's tool files bound to no message.
async fn unbound_tool_files(state: &SharedState) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_attachments WHERE source = 'tool' AND message_id IS NULL",
    )
    .fetch_one(&state.db)
    .await
    .unwrap()
}

/// Waits until `cond` holds, polling.
pub(super) async fn until<F, Fut>(what: &str, cond: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if cond().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("never: {what}");
}

/// A turn stopped while its picture is drawn (review finding 4): the call
/// ends itself on the turn's stop and still writes its rows — the image
/// row and the tool row, both `canceled` — rather than being dropped with
/// none.
#[tokio::test]
async fn a_picture_stopped_with_its_turn_still_writes_its_rows() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("generating")]).await;
    let chat = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    chat_model(
        &state,
        &chat,
        &[
            tool_call(
                "c1",
                "image__generate",
                json!({"model": "image/z", "prompt": "a red square"}),
            ),
            answer("never mind then"),
        ],
    )
    .await;
    let tid = thread(&gw).await;
    let first = tokio::spawn({
        let gw = gw.clone();
        async move { send(&gw, tid, "draw me a red square").await }
    });
    until("the job is submitted", || async {
        !super::submitted(&sd).await.is_empty()
    })
    .await;
    // A newer turn of the thread stops this one.
    let _ = send(&gw, tid, "never mind").await;
    let _ = first.await.unwrap();

    let image = rows_where(&state, (Some("image"), None), 1).await;
    assert_eq!(image, [(200, Some("canceled".to_string()))]);
    let tool = rows_where(&state, (None, Some("chat-tool")), 1).await;
    assert_eq!(tool, [(200, Some("canceled".to_string()))]);
}

/// A turn superseded after its picture was made (design §6.3): the reply is
/// not saved, so its file is dropped with it — no tool file is left bound
/// to no message.
#[tokio::test]
async fn a_superseded_turn_leaves_no_unbound_tool_file() {
    let sd = MockServer::start().await;
    mount_sd(&sd, &[job("completed")]).await;
    let chat = MockServer::start().await;
    let (state, gw) = gateway(&sd, &[image_row("z", Z_IMAGE, false)]).await;
    let sse = |b: String| {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_raw(b, "text/event-stream")
    };
    chat_model_answers(
        &state,
        &chat,
        vec![
            sse(tool_call(
                "c1",
                "image__generate",
                json!({"model": "image/z", "prompt": "a red square"}),
            )),
            // The model call after the picture: still running when the next
            // turn starts.
            sse(answer("too late")).set_delay(std::time::Duration::from_secs(30)),
            sse(answer("the newer turn")),
        ],
    )
    .await;
    let tid = thread(&gw).await;
    let first = tokio::spawn({
        let gw = gw.clone();
        async move { send(&gw, tid, "draw me a red square").await }
    });
    until(
        "the picture is stored and the model asked again",
        || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM chat_attachments WHERE source = 'tool'",
            )
            .fetch_one(&state.db)
            .await
            .unwrap()
                == 1
                && chat_bodies(&chat).await.len() == 2
        },
    )
    .await;
    let body = send(&gw, tid, "never mind").await;
    assert!(body.contains("the newer turn"), "{body}");
    let _ = first.await.unwrap();

    until("the superseded turn's file is dropped", || async {
        unbound_tool_files(&state).await == 0
    })
    .await;
    let d = detail(&gw, tid).await;
    let replies: Vec<&Value> = d["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .collect();
    assert_eq!(replies.len(), 1, "{d:#}");
    assert!(replies[0]["attachments"].as_array().unwrap().is_empty());
}
