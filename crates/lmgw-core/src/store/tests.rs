use super::*;

async fn pool() -> SqlitePool {
    open_in_memory().await.unwrap()
}

/// Insert a response, dating it `hours_ago` so the age rule can be tested
/// without waiting.
async fn seed(pool: &SqlitePool, id: &str, chain: &str, prev: Option<&str>, hours_ago: i64) {
    insert_response(
        pool,
        &StoredResponse {
            id: id.into(),
            chain_id: chain.into(),
            previous_response_id: prev.map(String::from),
            model: "m".into(),
            status: "completed".into(),
            body: "{}".into(),
            input_items: "[]".into(),
            messages: "[]".into(),
            pending: None,
            input_tokens: Some(1),
            output_tokens: Some(2),
            created_at: String::new(),
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE responses SET created_at = datetime('now', ?2) WHERE id = ?1")
        .bind(id)
        .bind(format!("-{hours_ago} hours"))
        .execute(pool)
        .await
        .unwrap();
}

/// The reason GC is chain-aware. Age-per-response would delete the root
/// first — it is always the oldest row — and leave a live conversation
/// unable to replay its own history.
#[tokio::test]
async fn gc_keeps_a_chain_alive_while_its_head_is_recent() {
    let pool = pool().await;
    // One conversation: an old root, extended a minute ago.
    seed(&pool, "r1", "r1", None, 100).await;
    seed(&pool, "r2", "r1", Some("r1"), 0).await;
    // And one that was abandoned days ago.
    seed(&pool, "old", "old", None, 100).await;

    let removed = gc_responses(&pool, 24, 0).await.unwrap();
    assert_eq!(removed, 1, "only the abandoned chain goes");
    assert!(get_response(&pool, "r1").await.unwrap().is_some());
    assert!(get_response(&pool, "r2").await.unwrap().is_some());
    assert!(get_response(&pool, "old").await.unwrap().is_none());
}

#[tokio::test]
async fn gc_by_chain_count_evicts_whole_conversations_least_recent_first() {
    let pool = pool().await;
    seed(&pool, "a1", "a", None, 5).await;
    seed(&pool, "a2", "a", Some("a1"), 4).await;
    seed(&pool, "b1", "b", None, 3).await;
    seed(&pool, "c1", "c", None, 1).await;

    gc_responses(&pool, 0, 2).await.unwrap();
    // "a" is the least recently active, and both of its rows go together.
    assert!(get_response(&pool, "a1").await.unwrap().is_none());
    assert!(get_response(&pool, "a2").await.unwrap().is_none());
    assert!(get_response(&pool, "b1").await.unwrap().is_some());
    assert!(get_response(&pool, "c1").await.unwrap().is_some());
}

/// Both rules off means keep everything — a visible choice, not a hidden
/// default that quietly drops data.
#[tokio::test]
async fn gc_with_both_rules_off_keeps_everything() {
    let pool = pool().await;
    seed(&pool, "ancient", "ancient", None, 10_000).await;
    assert_eq!(gc_responses(&pool, 0, 0).await.unwrap(), 0);
    assert!(get_response(&pool, "ancient").await.unwrap().is_some());
}

#[tokio::test]
async fn chains_are_summarized_from_their_newest_response() {
    let pool = pool().await;
    seed(&pool, "r1", "r1", None, 5).await;
    seed(&pool, "r2", "r1", Some("r1"), 1).await;
    let chains = list_response_chains(&pool, 10).await.unwrap();
    assert_eq!(chains.len(), 1);
    let c = &chains[0];
    assert_eq!(c.chain_id, "r1");
    assert_eq!(c.head_id, "r2", "the head is what a client continues from");
    assert_eq!(c.responses, 2);
    assert_eq!(c.input_tokens, 2);
    assert_eq!(c.output_tokens, 4);
    assert!(!c.awaiting_approval);
}
