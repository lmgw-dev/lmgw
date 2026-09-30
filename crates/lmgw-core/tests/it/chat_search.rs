//! Chat search (chat-complete design §4): the FTS index behind
//! `GET /chat/api/search` — what it finds, how it ranks and pages, and that
//! every way a row changes or goes away keeps it honest.

use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, ChatMessageUpdate, ThreadDefaults};
use serde_json::{json, Value};

use crate::chat_actions::post;
use crate::common::{serve, Gw};

async fn gw() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

/// A stored thread titled `title` (set directly, like a rename).
async fn thread(state: &SharedState, title: &str) -> i64 {
    let id = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    store::set_chat_thread_title(&state.db, id, title)
        .await
        .unwrap();
    id
}

async fn say(state: &SharedState, tid: i64, role: &str, text: &str) -> i64 {
    store::append_chat_message(&state.db, tid, role, text, "", None, None, None)
        .await
        .unwrap()
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `text`, optionally followed by `&key=value` extras (values are plain).
async fn search_raw(gw: &Gw, query: &str) -> reqwest::Response {
    let mut parts = query.split('&');
    let mut url = format!("{gw}/chat/api/search?q={}", enc(parts.next().unwrap()));
    for extra in parts {
        url.push('&');
        url.push_str(extra);
    }
    gw.client().get(url).send().await.unwrap()
}

/// `word` alone, or `word&archived=all&folder=3` style extras.
async fn search(gw: &Gw, query: &str) -> Value {
    let r = search_raw(gw, query).await;
    let status = r.status();
    let body: Value = r.json().await.unwrap();
    assert_eq!(status, 200, "search {query:?}: {body}");
    body
}

fn ids(v: &Value) -> Vec<i64> {
    v["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["thread_id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn titles_messages_and_sent_attachment_names_are_found() {
    let (state, gw) = gw().await;
    let a = thread(&state, "Quarterly planning").await;
    let b = thread(&state, "Other").await;
    let mid = say(&state, b, "user", "the budget spreadsheet is ready").await;
    let att = store::insert_chat_attachment(
        &state.db,
        b,
        "text",
        "roadmap.md",
        "text/markdown",
        3,
        b"abc",
    )
    .await
    .unwrap();

    let r = search(&gw, "planning").await;
    assert_eq!(ids(&r), vec![a]);
    assert_eq!(r["threads"][0]["hits"][0]["kind"], "t");

    let r = search(&gw, "spreadsheet").await;
    assert_eq!(ids(&r), vec![b]);
    let hit = &r["threads"][0]["hits"][0];
    assert_eq!(
        (hit["kind"].as_str(), hit["message_id"].as_i64()),
        (Some("m"), Some(mid))
    );
    assert_eq!(hit["role"], "user");

    // A draft is not indexed; sending binds it and brings its name in.
    assert!(ids(&search(&gw, "roadmap").await).is_empty());
    sqlx::query("UPDATE chat_attachments SET message_id = ?1 WHERE id = ?2")
        .bind(mid)
        .bind(att)
        .execute(&state.db)
        .await
        .unwrap();
    let r = search(&gw, "roadmap").await;
    assert_eq!(ids(&r), vec![b]);
    assert_eq!(r["threads"][0]["hits"][0]["kind"], "a");
    assert_eq!(r["threads"][0]["hits"][0]["message_id"], mid);
}

#[tokio::test]
async fn diacritics_prefix_and_phrase() {
    let (state, gw) = gw().await;
    let t = thread(&state, "x1").await;
    say(
        &state,
        t,
        "user",
        "Herr Müller starts the deployment tonight",
    )
    .await;
    let u = thread(&state, "x2").await;
    say(
        &state,
        u,
        "user",
        "the tonight deployment of Müller starts late",
    )
    .await;

    assert_eq!(ids(&search(&gw, "muller").await).len(), 2);
    assert_eq!(ids(&search(&gw, "MÜLLER").await).len(), 2);
    assert_eq!(ids(&search(&gw, "deplo").await).len(), 2, "prefix");
    let r = search(&gw, "\"deployment tonight\"").await;
    assert_eq!(ids(&r), vec![t], "a phrase is contiguous and in order");
    assert_eq!(
        ids(&search(&gw, "muller starts deplo").await).len(),
        2,
        "words are ANDed"
    );
    assert!(ids(&search(&gw, "muller zebra").await).is_empty());
}

#[tokio::test]
async fn the_more_relevant_thread_ranks_first() {
    let (state, gw) = gw().await;
    let weak = thread(&state, "weak").await;
    say(&state, weak, "user", "a long message that mentions kubernetes once among many many other unrelated words to dilute it").await;
    let strong = thread(&state, "kubernetes").await;
    say(&state, strong, "user", "kubernetes kubernetes").await;
    assert_eq!(ids(&search(&gw, "kubernetes").await), vec![strong, weak]);
}

#[tokio::test]
async fn snippets_carry_markers_and_hits_are_capped_and_counted() {
    let (state, gw) = gw().await;
    let t = thread(&state, "notes").await;
    for i in 0..5 {
        say(
            &state,
            t,
            "user",
            &format!("entry {i} about the zeppelin project"),
        )
        .await;
    }
    let r = search(&gw, "zepp").await;
    let th = &r["threads"][0];
    assert_eq!(th["match_count"], 5);
    assert_eq!(th["hits"].as_array().unwrap().len(), 3);
    let snip = th["hits"][0]["snippet"].as_str().unwrap();
    assert!(snip.contains("\u{E000}zeppelin\u{E001}"), "{snip:?}");
}

#[tokio::test]
async fn edits_reindex_and_deletes_unindex() {
    let (state, gw) = gw().await;
    let t = thread(&state, "old title").await;
    let m1 = say(&state, t, "user", "alpha content").await;
    let m2 = say(&state, t, "assistant", "bravo content").await;
    let m3 = say(&state, t, "user", "charlie content").await;

    store::update_chat_message(
        &state.db,
        t,
        m1,
        &ChatMessageUpdate {
            content: "delta content".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        ids(&search(&gw, "alpha").await).is_empty(),
        "the old text is gone"
    );
    assert_eq!(ids(&search(&gw, "delta").await), vec![t]);

    store::set_chat_thread_title(&state.db, t, "new heading")
        .await
        .unwrap();
    assert!(ids(&search(&gw, "old").await).is_empty());
    assert_eq!(ids(&search(&gw, "heading").await), vec![t]);

    store::delete_chat_message(&state.db, t, m2).await.unwrap();
    assert!(ids(&search(&gw, "bravo").await).is_empty());

    // Regenerate cuts from a message on.
    store::truncate_chat_messages(&state.db, t, m1, false)
        .await
        .unwrap();
    assert!(ids(&search(&gw, "charlie").await).is_empty());
    assert_eq!(ids(&search(&gw, "delta").await), vec![t]);
    let _ = m3;
}

#[tokio::test]
async fn deleting_a_thread_cascades_into_the_index() {
    let (state, gw) = gw().await;
    let t = thread(&state, "doomed").await;
    let m = say(&state, t, "user", "cascade me please").await;
    let att =
        store::insert_chat_attachment(&state.db, t, "text", "cascade.txt", "text/plain", 1, b"x")
            .await
            .unwrap();
    sqlx::query("UPDATE chat_attachments SET message_id = ?1 WHERE id = ?2")
        .bind(m)
        .bind(att)
        .execute(&state.db)
        .await
        .unwrap();
    assert_eq!(ids(&search(&gw, "cascade").await), vec![t]);
    let r = post(&gw, &format!("/chat/api/threads/{t}/delete"), json!({})).await;
    assert!(r.status().is_success());
    assert!(ids(&search(&gw, "cascade").await).is_empty());
    assert!(ids(&search(&gw, "doomed").await).is_empty());
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_fts WHERE thread_id = ?1")
        .bind(t)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(left, 0, "no orphan index rows");
}

#[tokio::test]
async fn folder_and_archived_filters() {
    let (state, gw) = gw().await;
    let f = store::create_chat_folder(&state.db, "Work", &ThreadDefaults::default())
        .await
        .unwrap();
    let in_folder = thread(&state, "wombat one").await;
    store::set_chat_thread_folder(&state.db, in_folder, Some(f))
        .await
        .unwrap();
    let loose = thread(&state, "wombat two").await;
    let old = thread(&state, "wombat three").await;
    store::archive_chat_thread(&state.db, old).await.unwrap();

    let mut active = ids(&search(&gw, "wombat").await);
    active.sort();
    assert_eq!(active, vec![in_folder, loose]);
    assert_eq!(
        ids(&search(&gw, &format!("wombat&folder={f}")).await),
        vec![in_folder]
    );
    assert_eq!(ids(&search(&gw, "wombat&archived=1").await), vec![old]);
    let r = search(&gw, "wombat&archived=all").await;
    assert_eq!(r["total_threads"], 3);
    let arch = r["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["thread_id"] == old)
        .unwrap();
    assert_eq!(arch["archived"], true);
    assert_eq!(search_raw(&gw, "wombat&archived=x").await.status(), 400);
}

#[tokio::test]
async fn results_are_paged_and_say_how_many_there_are() {
    let (state, gw) = gw().await;
    let n = store::SEARCH_PAGE_THREADS + 7;
    for i in 0..n {
        thread(&state, &format!("pagetest {i}")).await;
    }
    let p1 = search(&gw, "pagetest").await;
    assert_eq!(p1["total_threads"], n);
    assert_eq!(
        p1["threads"].as_array().unwrap().len() as i64,
        store::SEARCH_PAGE_THREADS
    );
    assert_eq!(p1["next_offset"], store::SEARCH_PAGE_THREADS);
    let p2 = search(
        &gw,
        &format!("pagetest&offset={}", store::SEARCH_PAGE_THREADS),
    )
    .await;
    assert_eq!(p2["threads"].as_array().unwrap().len(), 7);
    assert!(p2["next_offset"].is_null());
    let mut all = ids(&p1);
    all.extend(ids(&p2));
    all.sort();
    all.dedup();
    assert_eq!(all.len() as i64, n, "no thread on two pages");
}

#[tokio::test]
async fn temporary_threads_are_never_indexed() {
    let (state, gw) = gw().await;
    thread(&state, "stored quokka").await;
    let t: Value = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "temporary": true}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert!(t["id"].as_i64().unwrap() < 0);
    let title = t["title"].as_str().unwrap().to_string();
    let r = search(&gw, &title).await;
    assert!(
        !ids(&r).iter().any(|i| *i < 0),
        "a temporary thread never shows up"
    );
    assert_eq!(ids(&search(&gw, "quokka").await).len(), 1);
}

#[tokio::test]
async fn hostile_input_never_breaks_the_query() {
    let (state, gw) = gw().await;
    let t = thread(&state, "plain").await;
    say(&state, t, "user", "near foo-bar baz: qux").await;
    for q in [
        "\"\"\"",
        "\"\"",
        "**",
        "NEAR(",
        "NEAR(a b)",
        "a-b",
        "foo:bar",
        "\"open",
        "a\"b",
        "- -",
        "(x OR",
        "col:x*",
        "^x",
        "x AND",
        "{a}",
    ] {
        let r = search_raw(&gw, q).await;
        assert_eq!(r.status(), 200, "query {q:?}");
    }
    assert_eq!(ids(&search(&gw, "foo-bar").await), vec![t]);
    assert_eq!(ids(&search(&gw, "baz:").await), vec![t]);
    // Too short is a clear 400.
    for q in ["", " ", "a", " a "] {
        let r = search_raw(&gw, q).await;
        assert_eq!(r.status(), 400, "query {q:?}");
        assert!(r.json::<Value>().await.unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("at least 2"));
    }
}

/// A NUL or any other control character in the query separates words like
/// whitespace does — FTS5 read a NUL inside a quoted term as the end of its
/// string, an unterminated string and a 500 (review R1 finding 5).
#[tokio::test]
async fn control_characters_are_separators_not_errors() {
    let (state, gw) = gw().await;
    let t = thread(&state, "plain").await;
    say(&state, t, "user", "abacus under the cedar").await;
    for q in [
        "ab\0c",
        "\0ab",
        "ab\u{1}",
        "\u{1b}[31mab",
        "ab\u{7f}ced",
        "\"ab\0ced\"",
        "\0\0",
        "ab\tce\r\nx",
    ] {
        let r = search_raw(&gw, q).await;
        let status = r.status();
        let body: Value = r.json().await.unwrap();
        assert_eq!(status, 200, "query {q:?}: {body}");
    }
    assert_eq!(ids(&search(&gw, "abacus\0ced").await), vec![t]);
    assert!(
        ids(&search(&gw, "\0\0").await).is_empty(),
        "nothing searchable"
    );
}

/// A thread id or folder that is not a number is refused in the Chat API's
/// own flat `{code, message}` shape, not as axum's plain text (review R1
/// item d).
#[tokio::test]
async fn an_unreadable_query_is_a_flat_api_error() {
    let (_state, gw) = gw().await;
    let r = search_raw(&gw, "abc&folder=x").await;
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["code"], "bad_request", "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("folder"),
        "{body}"
    );
}

/// A page's hits come from one statement for the whole page rather than one
/// full MATCH per thread (review R1 finding 6): a page of 50 threads over a
/// few thousand matching messages is answered well within a second or two
/// even unoptimised, with every thread's three best hits and its count.
#[tokio::test]
async fn a_full_page_over_thousands_of_matches_stays_quick_and_complete() {
    let (state, gw) = gw().await;
    let threads = 60;
    let per_thread = 80;
    let mut tids = Vec::new();
    for i in 0..threads {
        tids.push(thread(&state, &format!("haystack {i}")).await);
    }
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < ?2) \
         INSERT INTO chat_messages (thread_id, role, content) \
         SELECT (SELECT value FROM json_each(?1) WHERE key = n.i % json_array_length(?1)), \
                CASE n.i % 2 WHEN 0 THEN 'user' ELSE 'assistant' END, \
                'a needle ' || n.i || ' in a long enough haystack of words ' || \
                CASE n.i % 7 WHEN 0 THEN 'needle needle' ELSE '' END \
         FROM n",
    )
    .bind(serde_json::to_string(&tids).unwrap())
    .bind(threads * per_thread)
    .execute(&state.db)
    .await
    .unwrap();

    let t0 = std::time::Instant::now();
    let r = search(&gw, "needle").await;
    let took = t0.elapsed();
    eprintln!("search over {} messages: {took:?}", threads * per_thread);
    assert_eq!(r["total_threads"], threads);
    let page = r["threads"].as_array().unwrap();
    assert_eq!(page.len() as i64, store::SEARCH_PAGE_THREADS);
    for t in page {
        assert_eq!(t["match_count"], per_thread, "{t}");
        let hits = t["hits"].as_array().unwrap();
        assert_eq!(hits.len() as i64, store::SEARCH_HITS_PER_THREAD, "{t}");
        for h in hits {
            assert_eq!(h["kind"], "m");
            assert!(h["message_id"].is_i64());
            assert!(
                matches!(h["role"].as_str(), Some("user" | "assistant")),
                "{h}"
            );
            assert!(h["snippet"].as_str().unwrap().contains('\u{E000}'), "{h}");
        }
        // The best hit is one that says it three times.
        let best = hits[0]["snippet"].as_str().unwrap();
        assert!(best.matches('\u{E000}').count() >= 3, "{best}");
    }
    assert!(
        took < std::time::Duration::from_secs(5),
        "a page took {took:?}"
    );
}
