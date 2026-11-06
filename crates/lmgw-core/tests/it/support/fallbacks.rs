//! Fallbacks as tests set them (openai egress design §14.2): each model and
//! alias has its own; there is no global one. What a test that once set the
//! global hold fallback means is what migration 0095 wrote for every row
//! that followed it: every chat row, and every candidate alias without one of
//! its own, falls back to that name.

use lmgw_core::state::SharedState;

/// Every chat row, and every candidate alias that has none of its own,
/// falls back to `name` (`None`: every chat row has none); then the
/// snapshot is reloaded.
pub async fn every_chat_model_falls_back_to(state: &SharedState, name: Option<&str>) {
    sqlx::query("UPDATE local_models SET fallback = ?1")
        .bind(name)
        .execute(&state.db)
        .await
        .unwrap();
    if name.is_some() {
        sqlx::query("UPDATE candidate_aliases SET fallback = ?1 WHERE fallback IS NULL")
            .bind(name)
            .execute(&state.db)
            .await
            .unwrap();
    }
    state.reload_snapshot().await.unwrap();
}

/// The model or alias `owner` names falls back to `fallback`, written
/// straight to its row (`table` is `local_models`, `aux_models`,
/// `audio_models`, `image_models`, `models` or `candidate_aliases`; `key`
/// the row's `model_id`, or `alias` for the two alias tables); then the
/// snapshot is reloaded.
pub async fn set_fallback(state: &SharedState, table: &str, owner: &str, fallback: Option<&str>) {
    let key = match table {
        "models" | "candidate_aliases" => "alias",
        _ => "model_id",
    };
    let sql = format!("UPDATE {table} SET fallback = ?2 WHERE {key} = ?1");
    let done = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(owner)
        .bind(fallback)
        .execute(&state.db)
        .await
        .unwrap();
    assert_eq!(done.rows_affected(), 1, "no {table} row '{owner}'");
    state.reload_snapshot().await.unwrap();
}
