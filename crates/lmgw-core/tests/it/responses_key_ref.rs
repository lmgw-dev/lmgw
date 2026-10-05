//! `/v1/responses` records every turn against the key it is (A2 review 4):
//! a run that outlives a rename of its key keeps the key's id, and its row
//! the key's current name — not the name it had at the request's start,
//! which no key has any more, or a new key that took it.

use std::sync::Arc;

use lmgw_core::config::KeyPolicy;
use serde_json::json;
use tokio::sync::Notify;

use crate::support::realtime_fakes::{chat_fake, gateway, Turn, KEY};

#[tokio::test]
async fn a_turn_that_outlives_a_rename_of_its_key_is_the_key_s() {
    let fake = chat_fake().await;
    let go = Arc::new(Notify::new());
    fake.push(Turn::Held(go.clone(), Box::new(Turn::text(&["Hi."]))));
    let (state, addr) = gateway(&fake, true, Some(KeyPolicy::default()), |_| {}).await;
    let (id,): (i64,) = sqlx::query_as("SELECT id FROM api_keys WHERE name = 'voice'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://{addr}/v1/responses"))
            .bearer_auth(KEY)
            .json(&json!({"model": "chatty", "input": "hi", "stream": true}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    for _ in 0..500 {
        if fake.seen.chat_count() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(fake.seen.chat_count(), 1, "the turn is with the upstream");
    // Renamed while the turn runs, and a new key takes the old name.
    sqlx::query("UPDATE api_keys SET name = 'voice-renamed' WHERE id = ?1")
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('voice', 'other', 1)")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    go.notify_one();
    let body = request.await.unwrap();
    assert!(body.contains("response.completed"), "{body}");

    let mut rows: Vec<(Option<String>, Option<i64>)> = Vec::new();
    for _ in 0..500 {
        rows = sqlx::query_as(
            "SELECT client_key, key_id FROM request_logs WHERE requested_alias = 'chatty'",
        )
        .fetch_all(&state.db)
        .await
        .unwrap();
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(rows, [(Some("voice-renamed".into()), Some(id))]);
}
