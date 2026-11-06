//! `lmgw__hf_downloads` and `GET /api/hf/downloads`: the `ids` and `active`
//! filters, and the compact text block every self-admin tool answers with.

use super::*;

/// One row per status a download can be in: queued, downloading, done,
/// failed, update_available. Returns their ids in that order.
async fn download_rows(state: &SharedState) -> Vec<i64> {
    let mut ids = Vec::new();
    for (n, status) in [
        "queued",
        "downloading",
        "done",
        "failed",
        "update_available",
    ]
    .into_iter()
    .enumerate()
    {
        let file = format!("f{n}.gguf");
        let id = store::upsert_hf_model(&state.db, "p/r", &file, &format!("p/r/{file}"), "chat")
            .await
            .unwrap();
        store::set_hf_status(&state.db, id, status, None)
            .await
            .unwrap();
        ids.push(id);
    }
    ids
}

fn listed_ids(p: &Value) -> Vec<i64> {
    let mut ids: Vec<i64> = p["downloads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_i64().unwrap())
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn the_tool_filters_by_ids_and_by_active() {
    let state = state_with_mode(SelfAdmin::ReadOnly).await;
    let rows = download_rows(&state).await;
    let base = serve_with(state).await;
    let sid = initialize(&base).await;
    let tool = "lmgw__hf_downloads";

    // No arguments: every row, in one compact block (no indentation to pay for).
    let all = call(&base, &sid, tool, json!({})).await;
    assert!(!text_of(&all).contains('\n'), "{}", text_of(&all));
    assert_eq!(listed_ids(&payload(&all)), rows);

    let some = call(&base, &sid, tool, json!({ "ids": [rows[0], rows[2]] })).await;
    assert_eq!(listed_ids(&payload(&some)), vec![rows[0], rows[2]]);

    // Active: queued, downloading and failed; done and update_available are finished.
    let active = call(&base, &sid, tool, json!({ "active": true })).await;
    assert_eq!(
        listed_ids(&payload(&active)),
        vec![rows[0], rows[1], rows[3]]
    );
    let inactive = call(&base, &sid, tool, json!({ "active": false })).await;
    assert_eq!(listed_ids(&payload(&inactive)), rows);

    // Both: a row has to pass both.
    let both = call(
        &base,
        &sid,
        tool,
        json!({ "ids": [rows[0], rows[2]], "active": true }),
    )
    .await;
    assert_eq!(listed_ids(&payload(&both)), vec![rows[0]]);

    // An empty list asks for no rows, not for every row.
    let none = call(&base, &sid, tool, json!({ "ids": [] })).await;
    assert!(listed_ids(&payload(&none)).is_empty());
}

#[tokio::test]
async fn the_tool_refuses_a_wrong_typed_filter() {
    let base = serve_with(state_with_mode(SelfAdmin::ReadOnly).await).await;
    let sid = initialize(&base).await;
    let ids_msg = "'ids' must be an array of integers";
    for (args, want) in [
        (json!({ "ids": "1,2" }), ids_msg),
        (json!({ "ids": [1, "2"] }), ids_msg),
        (json!({ "ids": [1.5] }), ids_msg),
        (json!({ "active": "yes" }), "'active' must be a boolean"),
    ] {
        let r = call(&base, &sid, "lmgw__hf_downloads", args.clone()).await;
        assert!(is_error(&r), "{args}: {}", text_of(&r));
        assert!(text_of(&r).contains(want), "{args}: {}", text_of(&r));
    }
}

#[tokio::test]
async fn the_http_route_takes_ids_and_active_as_query_parameters() {
    let state = AppState::init_for_tests().await.unwrap();
    let rows = download_rows(&state).await;
    let gw = crate::common::serve(state).await;
    let get = |query: String| {
        let gw = &gw;
        async move {
            gw.client()
                .get(format!("{gw}/api/hf/downloads{query}"))
                .send()
                .await
                .unwrap()
        }
    };
    let ok = |query: String| {
        let get = get(query);
        async move {
            let resp = get.await;
            assert_eq!(resp.status().as_u16(), 200);
            listed_ids(&resp.json::<Value>().await.unwrap())
        }
    };

    assert_eq!(ok(String::new()).await, rows);
    assert_eq!(
        ok(format!("?ids={},{}", rows[1], rows[2])).await,
        vec![rows[1], rows[2]]
    );
    assert_eq!(
        ok("?active=true".into()).await,
        vec![rows[0], rows[1], rows[3]]
    );
    assert_eq!(
        ok(format!("?active=true&ids={},{}", rows[2], rows[3])).await,
        vec![rows[3]]
    );
    assert!(ok("?ids=".into()).await.is_empty());

    // A bad id is refused, not skipped.
    let resp = get("?ids=1,x".into()).await;
    assert_eq!(resp.status().as_u16(), 400);
    assert!(resp.text().await.unwrap().contains("not an integer"));
}
