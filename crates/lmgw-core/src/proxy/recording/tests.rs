//! What `price_call` folds into the policy's windows (A2 review 3, B2 review
//! 9): a key that exists gets its tokens; one deleted while its call ran
//! gets no window — nothing would ever read it again.

use super::{price_call, KeyRef};
use crate::ir::Usage;
use crate::state::AppState;

#[tokio::test]
async fn a_key_deleted_while_its_call_ran_gets_no_token_window() {
    let state = AppState::init_for_tests().await.unwrap();
    sqlx::query("INSERT INTO api_keys (name, key_hash, enabled) VALUES ('live', 'h', 1)")
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
    let (live,): (i64,) = sqlx::query_as("SELECT id FROM api_keys WHERE name = 'live'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let usage = Usage {
        prompt_tokens: Some(10),
        completion_tokens: Some(5),
        ..Default::default()
    };
    let call = |key: KeyRef| price_call(&state, "chatty", None, None, &key, "openai", &usage);

    call(KeyRef {
        name: Some("live".into()),
        id: Some(live),
    });
    assert!(
        state.policy.tracks(live),
        "a key that exists counts its tokens"
    );

    // The key's id outlives it on the row (`KeyRef`), but not in the gate.
    let gone = live + 1000;
    let priced = call(KeyRef {
        name: Some("gone".into()),
        id: Some(gone),
    });
    assert_eq!(priced.key_id, Some(gone), "the row keeps the id");
    assert_eq!(priced.client_key.as_deref(), Some("gone"));
    assert!(!state.policy.tracks(gone), "no window for a deleted key");
}
