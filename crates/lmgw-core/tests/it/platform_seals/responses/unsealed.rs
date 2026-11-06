//! A loop step whose encrypted reasoning never came (a fallback answered
//! it, or the platform sent none): the loop writes its reasoning item with
//! a summary alone, under an id it minted. Taken to the native path, that
//! item is left out of the body (WP8 verify V-F1), for a client key and the
//! owner alike: the platform could not use it, and its made-up id is no
//! item of anyone's to refuse or look up.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use wiremock::Request;

use super::{as_key, in_the_loop};
use crate::platform_seals::{answer, client_key, platform, posted};
use crate::support::responses_wire as rw;

/// A reasoning item as the platform writes one it sent no encrypted
/// content for.
fn unsealed(id: &str, summary: &str) -> Value {
    let mut item = rw::reasoning(id, summary);
    item.as_object_mut().unwrap().remove("encrypted_content");
    item
}

#[tokio::test]
async fn a_loop_step_without_encrypted_reasoning_is_left_out_on_the_native_path() {
    let n = Arc::new(AtomicUsize::new(0));
    let (mock, state, gw, _up) = platform(move |req: &Request| {
        let items = match n.fetch_add(1, Ordering::Relaxed) {
            0 => vec![
                unsealed("rs_1", "Weighing it."),
                rw::message("msg_1", "Hello"),
            ],
            _ => vec![rw::message("msg_n", "Fine.")],
        };
        answer(req, &items)
    })
    .await;
    client_key(&state, "k1").await;
    let first: Value = as_key(&gw, Some("k1"), in_the_loop(json!("hi"), false))
        .await
        .json()
        .await
        .unwrap();
    let output: Vec<Value> = first["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "reasoning" || i["type"] == "message")
        .cloned()
        .collect();
    assert_eq!(output[0]["type"], "reasoning", "{first}");
    assert!(
        output[0].get("encrypted_content").is_none(),
        "no seal came: {first}"
    );
    assert_eq!(output[0]["summary"][0]["text"], "Weighing it.");

    // The transcript, stateless and native, as the client key and as the
    // owner: the reasoning item is not sent, the message goes without the
    // id the loop made up.
    let mut input = vec![json!({"role": "user", "content": "hi"})];
    input.extend(output);
    input.push(json!({"role": "user", "content": "and?"}));
    let body = json!({"model": "my-model", "store": false, "input": input});
    let r = as_key(&gw, Some("k1"), body.clone()).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let r = crate::chat_actions::post(&gw, "/v1/responses", body).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let sent = posted(&mock).await;
    assert_eq!(sent.len(), 3);
    for native in &sent[1..] {
        assert_eq!(
            native["input"],
            json!([
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "Hello"},
                {"role": "user", "content": "and?"},
            ]),
            "{native}"
        );
    }
}
