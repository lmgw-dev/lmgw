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

/// A wait for the write lock past half the busy timeout opens a contention
/// episode with a warning that names the caller's place, the wait, the
/// threshold and the timeout, and says it names the waiter. The writers
/// queued with it are said at debug, and the episode's end says how many
/// waited past the threshold, the longest and how many gave up, once none
/// waits (review B-5: one slow holder warned once per writer queued behind
/// it).
#[test]
fn a_contention_for_the_write_lock_is_one_warning_and_its_end() {
    use write_lock::{Contention, Outcome, Said};
    let first = std::panic::Location::caller();
    let (queued, late) = (queued_at(), late_at());
    let long = BUSY_TIMEOUT / 2 + Duration::from_millis(1);
    let mut c = Contention::default();
    for _ in 0..3 {
        c.start();
    }

    let said = c.end("lmgw.sqlite", long, first, Outcome::GotIt);
    let [Said::Warn(warn)] = said.as_slice() else {
        panic!("{said:?}");
    };
    for words in [
        &first.to_string(),
        "waited 2501 ms",
        "and got it",
        "warns past 2500 ms",
        "busy timeout of 5000 ms",
        "This names the waiter",
        "on lmgw.sqlite",
        "2 more writers wait for it now",
    ] {
        assert!(warn.contains(words), "{words:?} in {warn}");
    }
    let said = c.end("lmgw.sqlite", BUSY_TIMEOUT, queued, Outcome::GaveUp);
    assert!(
        matches!(said.as_slice(), [Said::Debug(d)] if d.contains("waited 5000 ms") && d.contains("gave up")),
        "{said:?}"
    );
    // The last one waits a little past the threshold, and ends the episode.
    let said = c.end("lmgw.sqlite", long, late, Outcome::GotIt);
    let [Said::Debug(_), Said::Warn(end)] = said.as_slice() else {
        panic!("{said:?}");
    };
    for words in [
        &first.to_string(),
        "has cleared: 3 write transactions waited past 2500 ms",
        &format!("the longest 5000 ms at {queued}"),
        "1 of them gave up",
    ] {
        assert!(end.contains(words), "{words:?} in {end}");
    }

    // A new episode warns again; one wait alone says only its warning, and
    // a short wait only its debug line.
    c.start();
    let said = c.end("lmgw.sqlite", long, late, Outcome::GotIt);
    assert!(
        matches!(said.as_slice(), [Said::Warn(w)] if w.contains("0 more writers")),
        "{said:?}"
    );
    c.start();
    let said = c.end(
        "lmgw.sqlite",
        Duration::from_millis(3),
        late,
        Outcome::GotIt,
    );
    assert!(
        matches!(said.as_slice(), [Said::Debug(d)] if d.contains("waited 3 ms")),
        "{said:?}"
    );
}

/// A writer whose caller went away mid-wait ends its wait as dropped (the
/// begin-write re-check's R-5): its count goes back, and the contention
/// episode it was in closes with the last waiter, so the database's entry
/// goes. A count that leaked would have kept that episode open for good,
/// and every later wait on that database would have been said at debug only.
#[test]
fn a_waiter_dropped_mid_wait_gives_its_count_back_and_closes_the_episode() {
    use write_lock::{contention_of, Outcome, Waiting};
    let db = std::path::PathBuf::from(format!("r5-dropped-{}.sqlite", std::process::id()));
    let at = std::panic::Location::caller();
    let dropped = Waiting::start(db.clone(), at);
    let long =
        Waiting::start(db.clone(), at).started_ago(BUSY_TIMEOUT / 2 + Duration::from_millis(1));
    assert_eq!(contention_of(&db), Some((2, false)));
    // The long wait opens an episode; the other writer still waits in it.
    long.end(Outcome::GotIt);
    assert_eq!(contention_of(&db), Some((1, true)));
    drop(dropped);
    assert_eq!(contention_of(&db), None, "the count and the episode leaked");
}

/// The same through `begin_write`: its future dropped while it waits for a
/// lock another transaction holds (a request whose client hung up) gives
/// its count back.
#[tokio::test]
async fn a_begin_write_dropped_while_it_waits_gives_its_count_back() {
    use write_lock::contention_of;
    let dir = tempfile::tempdir().unwrap();
    let pool = open(&dir.path().join("lmgw.sqlite")).await.unwrap();
    let db = pool.connect_options().get_filename().to_path_buf();
    let holder = begin_write(&pool).await.unwrap();
    assert_eq!(contention_of(&db), None, "the holder's wait ended");

    let mut waiter = Box::pin(begin_write(&pool));
    tokio::time::timeout(Duration::from_millis(200), &mut waiter)
        .await
        .expect_err("it waits for the holder's lock");
    assert_eq!(contention_of(&db), Some((1, false)));
    drop(waiter);
    assert_eq!(contention_of(&db), None, "the dropped wait's count leaked");
    holder.rollback().await.unwrap();
}

#[track_caller]
fn queued_at() -> &'static std::panic::Location<'static> {
    std::panic::Location::caller()
}

#[track_caller]
fn late_at() -> &'static std::panic::Location<'static> {
    std::panic::Location::caller()
}

/// The corpus stores lmgw opens through quickdoc-core wait for a lock as
/// long as this store does (review B-8): two constants, one value.
#[test]
fn the_corpus_store_waits_for_a_lock_as_long_as_this_one() {
    assert_eq!(BUSY_TIMEOUT, quickdoc_core::store::BUSY_TIMEOUT);
}

/// With no settings row, the gateway's level as the store reads it is the
/// one the snapshot loads: one source for the defaults (review B-9).
#[tokio::test]
async fn no_settings_row_reads_the_snapshot_s_default_level() {
    let pool = pool().await;
    sqlx::query("DELETE FROM settings WHERE key = 'settings'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        gateway_self_admin_now(&pool).await.unwrap(),
        load_settings(&pool).await.unwrap().self_admin
    );
    assert_eq!(
        load_settings(&pool).await.unwrap().self_admin,
        crate::config::Settings::default().self_admin
    );
}
