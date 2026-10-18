//! A request row says which listener its request came in on (device link
//! design §4.5): `devices` for a device's, `plain` for the owner's.

use serde_json::json;

use super::world;
use crate::support::device_pair;

/// The `via` of the newest request row.
async fn newest_via(w: &super::World) -> String {
    sqlx::query_scalar("SELECT via FROM request_logs ORDER BY id DESC LIMIT 1")
        .fetch_one(&w.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_request_row_says_which_listener_it_came_in_on() {
    let w = world().await;
    let phone = device_pair::pair(&w.state, &w.gw, "phone", json!({})).await;
    let ask = json!({ "model": "no-such-alias", "messages": [{"role": "user", "content": "hi"}] });

    let resp = phone
        .client
        .post(format!("{}/v1/chat/completions", w.gw))
        .json(&ask)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "{}", resp.status());
    crate::common::patience::until_async("the device's row is written", || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_logs WHERE key_id = ?1")
            .bind(phone.id)
            .fetch_one(&w.state.db)
            .await
            .unwrap()
            > 0
    })
    .await;
    assert_eq!(newest_via(&w).await, "devices");

    let resp =
        w.gw.client()
            .post(format!("{}/v1/chat/completions", w.gw))
            .json(&ask)
            .send()
            .await
            .unwrap();
    assert!(resp.status().is_client_error(), "{}", resp.status());
    crate::common::patience::until_async("the owner's row is written", || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_logs")
            .fetch_one(&w.state.db)
            .await
            .unwrap()
            > 1
    })
    .await;
    assert_eq!(newest_via(&w).await, "plain");
}
