//! A reasoning item with encrypted content but no id (WP8 verify V-N1): no
//! recorded item vouches for its encrypted reasoning, and OpenAI requires a
//! reasoning input item's id, so a client key's is a 400 naming the item
//! before anything goes upstream; the owner's goes on, for the platform to
//! answer.

use serde_json::json;

use super::{as_key, numbered};
use crate::platform_seals::{client_key, posted, SEALED};

#[tokio::test]
async fn a_client_key_s_encrypted_reasoning_without_its_id_is_refused() {
    let (mock, state, gw, _up) = numbered().await;
    client_key(&state, "k1").await;
    let body = json!({"model": "my-model", "store": false, "input": [
        {"role": "user", "content": "hi"},
        {"type": "reasoning", "summary": [], "encrypted_content": SEALED},
        {"role": "assistant", "content": "Hello"},
        {"role": "user", "content": "and?"}]});
    let r = as_key(&gw, Some("k1"), body.clone()).await;
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("input[1]") && message.contains("no id"),
        "{v}"
    );
    assert!(posted(&mock).await.is_empty(), "nothing went upstream");

    let r = crate::chat_actions::post(&gw, "/v1/responses", body).await;
    assert_eq!(r.status(), 200, "the owner's goes on");
    assert_eq!(posted(&mock).await.len(), 1);
}
