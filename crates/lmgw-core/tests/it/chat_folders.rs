//! Chat folders (chat-complete design §5): one level of grouping, defaults a
//! new thread starts from (as a copy), the move, and both ways to delete.

use lmgw_core::state::AppState;
use lmgw_core::store::{self, ThreadDefaults, ThreadListMode};
use serde_json::{json, Value};

use crate::chat_actions::{get_json, post};
use crate::common::{serve, Gw};

async fn gw() -> (lmgw_core::state::SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    (state, gw)
}

async fn folder(gw: &Gw, name: &str, defaults: Value) -> Value {
    let r = post(
        gw,
        "/chat/api/folders",
        json!({"name": name, "defaults": defaults}),
    )
    .await;
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn thread_in(gw: &Gw, folder_id: Option<i64>) -> Value {
    let r = post(
        gw,
        "/chat/api/threads",
        json!({"model_alias": "global-m", "folder_id": folder_id}),
    )
    .await;
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

#[tokio::test]
async fn folders_are_created_renamed_sorted_and_listed_with_counts() {
    let (_state, gw) = gw().await;
    let a = folder(&gw, "  Work ", json!({})).await;
    let b = folder(&gw, "Home", json!({})).await;
    assert_eq!(a["name"], "Work", "the name is trimmed");
    assert!(
        b["sort"].as_i64() > a["sort"].as_i64(),
        "new folders go last"
    );

    let bid = b["id"].as_i64().unwrap();
    let r = post(
        &gw,
        &format!("/chat/api/folders/{bid}"),
        json!({"name": "Family", "sort": -5}),
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["name"], "Family");

    let t = thread_in(&gw, Some(bid)).await;
    let t2 = thread_in(&gw, Some(bid)).await;
    post(
        &gw,
        &format!("/chat/api/threads/{}/archive", t2["id"]),
        json!({"archived": true}),
    )
    .await;
    let list = get_json(&gw, "/chat/api/threads").await;
    let folders = list["folders"].as_array().unwrap();
    assert_eq!(folders[0]["name"], "Family", "sort orders the list: {list}");
    assert_eq!(folders[0]["threads_active"], 1);
    assert_eq!(folders[0]["threads_archived"], 1);
    assert_eq!(folders[1]["threads_active"], 0);
    assert_eq!(t["folder_id"], bid);
    let threads = list["threads"].as_array().unwrap();
    assert_eq!(
        threads[0]["folder_id"], bid,
        "thread JSON carries folder_id"
    );
    // Every list mode carries the folders.
    let arch = get_json(&gw, "/chat/api/threads?archived=1").await;
    assert_eq!(arch["folders"].as_array().unwrap().len(), 2);

    let solo: Value = get_json(&gw, "/chat/api/folders").await;
    assert_eq!(solo["folders"].as_array().unwrap().len(), 2);

    let blank = post(&gw, "/chat/api/folders", json!({"name": "   "})).await;
    assert_eq!(blank.status(), 400);
    let missing = post(&gw, "/chat/api/folders/999", json!({"name": "x"})).await;
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn a_new_thread_in_a_folder_copies_its_defaults_and_stays_a_copy() {
    let (state, gw) = gw().await;
    let f = folder(
        &gw,
        "Local",
        json!({
            "model_alias": "folder-m",
            "system_prompt": "Be terse.",
            "temperature": 0.3,
            "top_k": 40,
            "stop": ["END", ""],
            "reasoning_effort": " high ",
            "mcp_tools": [{"server_label": "srv", "allowed_tools": ["a"]}],
        }),
    )
    .await;
    assert_eq!(f["defaults"]["stop"], json!(["END"]), "normalised on save");
    assert_eq!(f["defaults"]["reasoning_effort"], "high");
    let fid = f["id"].as_i64().unwrap();

    let t = thread_in(&gw, Some(fid)).await;
    assert_eq!(t["model_alias"], "folder-m", "the folder's model wins");
    assert_eq!(t["system_prompt"], "Be terse.");
    assert_eq!(t["temperature"], 0.3);
    assert_eq!(t["top_k"], 40);
    assert_eq!(t["stop"], json!(["END"]));
    assert_eq!(t["mcp_tools"][0]["server_label"], "srv");
    assert_eq!(t["folder_id"], fid);

    // A field the folder leaves unset keeps the global start.
    let empty = folder(&gw, "Empty tools", json!({"mcp_tools": [], "stop": [""]})).await;
    assert_eq!(empty["defaults"]["mcp_tools"], Value::Null, "{empty}");
    assert_eq!(empty["defaults"]["stop"], Value::Null, "{empty}");
    let plain = folder(&gw, "Plain", json!({})).await;
    let p = thread_in(&gw, plain["id"].as_i64()).await;
    assert_eq!(p["model_alias"], "global-m");
    assert_eq!(p["temperature"], Value::Null);

    // Later folder changes never reach the existing thread.
    let r = post(
        &gw,
        &format!("/chat/api/folders/{fid}"),
        json!({"defaults": {"system_prompt": "Be verbose."}}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let after = get_json(&gw, &format!("/chat/api/threads/{}", t["id"])).await;
    assert_eq!(after["thread"]["system_prompt"], "Be terse.");
    assert_eq!(after["thread"]["top_k"], 40);
    let next = thread_in(&gw, Some(fid)).await;
    assert_eq!(next["system_prompt"], "Be verbose.");
    assert_eq!(next["top_k"], Value::Null, "defaults were replaced whole");

    // An unknown folder is a 404 and creates nothing.
    let n = store::list_chat_threads(&state.db, ThreadListMode::All)
        .await
        .unwrap()
        .len();
    let r = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "folder_id": 999}),
    )
    .await;
    assert_eq!(r.status(), 404);
    assert_eq!(
        store::list_chat_threads(&state.db, ThreadListMode::All)
            .await
            .unwrap()
            .len(),
        n
    );
}

#[tokio::test]
async fn invalid_defaults_are_refused_by_name_and_nothing_is_stored() {
    let (_state, gw) = gw().await;
    for (defaults, name) in [
        (json!({"top_p": 1.5}), "top_p"),
        (json!({"top_k": -1}), "top_k"),
        (json!({"presence_penalty": 3.0}), "presence_penalty"),
        (
            json!({"reasoning_enabled": true, "reasoning_effort": "none"}),
            "reasoning",
        ),
        (json!({"reasoning_budget": -4}), "reasoning budget"),
        (json!({"no_such_field": 1}), "no_such_field"),
    ] {
        let r = post(
            &gw,
            "/chat/api/folders",
            json!({"name": "x", "defaults": defaults}),
        )
        .await;
        assert_eq!(r.status(), 400, "{defaults}");
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["code"], "bad_request", "{body}");
        let _ = name;
    }
    let ok = folder(&gw, "ok", json!({})).await;
    let r = post(
        &gw,
        &format!("/chat/api/folders/{}", ok["id"]),
        json!({"defaults": {"min_p": 2.0}}),
    )
    .await;
    assert_eq!(r.status(), 400);
    let list = get_json(&gw, "/chat/api/folders").await;
    assert_eq!(list["folders"].as_array().unwrap().len(), 1);
    assert_eq!(list["folders"][0]["defaults"]["min_p"], Value::Null);
}

#[test]
fn stored_defaults_tolerate_unknown_fields_but_input_does_not() {
    let d = ThreadDefaults::from_stored(r#"{"top_k": 5, "from_the_future": [1]}"#);
    assert_eq!(d.top_k, Some(5));
    assert_eq!(
        ThreadDefaults::from_stored("not json"),
        ThreadDefaults::default()
    );
    assert!(serde_json::from_str::<ThreadDefaults>(r#"{"from_the_future": 1}"#).is_err());
}

#[tokio::test]
async fn move_files_a_thread_and_a_temporary_one_refuses() {
    let (_state, gw) = gw().await;
    let f = folder(&gw, "F", json!({"system_prompt": "folder prompt"})).await;
    let fid = f["id"].as_i64().unwrap();
    let t = thread_in(&gw, None).await;
    let tid = t["id"].as_i64().unwrap();

    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/move"),
        json!({"folder_id": fid}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let moved: Value = r.json().await.unwrap();
    assert_eq!(moved["folder_id"], fid);
    assert_ne!(
        moved["system_prompt"], "folder prompt",
        "moving copies no defaults"
    );

    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/move"),
        json!({"folder_id": null}),
    )
    .await;
    assert_eq!(r.json::<Value>().await.unwrap()["folder_id"], Value::Null);

    let r = post(
        &gw,
        &format!("/chat/api/threads/{tid}/move"),
        json!({"folder_id": 999}),
    )
    .await;
    assert_eq!(r.status(), 404);
    let r = post(
        &gw,
        "/chat/api/threads/9999/move",
        json!({"folder_id": null}),
    )
    .await;
    assert_eq!(r.status(), 404);

    let temp: Value = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "temporary": true}),
    )
    .await
    .json()
    .await
    .unwrap();
    let r = post(
        &gw,
        &format!("/chat/api/threads/{}/move", temp["id"]),
        json!({"folder_id": fid}),
    )
    .await;
    assert_eq!(r.status(), 409);
    assert_eq!(r.json::<Value>().await.unwrap()["code"], "temporary_thread");
    let r = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "temporary": true, "folder_id": fid}),
    )
    .await;
    assert_eq!(r.status(), 400);
    assert_eq!(temp["folder_id"], Value::Null);
}

#[tokio::test]
async fn deleting_a_folder_keeps_or_deletes_its_threads() {
    let (state, gw) = gw().await;
    for (fate, survives) in [("keep", true), ("delete", false)] {
        let f = folder(&gw, fate, json!({})).await;
        let fid = f["id"].as_i64().unwrap();
        let t = thread_in(&gw, Some(fid)).await;
        let tid = t["id"].as_i64().unwrap();
        store::append_chat_message(&state.db, tid, "user", "hi", "", None, None, None)
            .await
            .unwrap();
        // An unknown choice is refused before anything happens.
        let bad = post(
            &gw,
            &format!("/chat/api/folders/{fid}/delete"),
            json!({"threads": "maybe"}),
        )
        .await;
        assert!(bad.status().is_client_error());
        let none = post(&gw, &format!("/chat/api/folders/{fid}/delete"), json!({})).await;
        assert!(none.status().is_client_error(), "the choice is required");
        assert!(store::get_chat_folder(&state.db, fid)
            .await
            .unwrap()
            .is_some());

        let r = post(
            &gw,
            &format!("/chat/api/folders/{fid}/delete"),
            json!({"threads": fate}),
        )
        .await;
        assert_eq!(r.status(), 200, "{fate}");
        assert!(store::get_chat_folder(&state.db, fid)
            .await
            .unwrap()
            .is_none());
        let got = store::get_chat_thread(&state.db, tid).await.unwrap();
        assert_eq!(got.is_some(), survives, "{fate}");
        if let Some(got) = got {
            assert_eq!(got.folder_id, None);
        } else {
            assert!(store::list_chat_messages(&state.db, tid)
                .await
                .unwrap()
                .is_empty());
        }
    }
    let r = post(
        &gw,
        "/chat/api/folders/999/delete",
        json!({"threads": "keep"}),
    )
    .await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn keeping_a_temporary_thread_puts_it_in_no_folder() {
    let (_state, gw) = gw().await;
    let temp: Value = post(
        &gw,
        "/chat/api/threads",
        json!({"model_alias": "m", "temporary": true}),
    )
    .await
    .json()
    .await
    .unwrap();
    let r = post(
        &gw,
        &format!("/chat/api/threads/{}/persist", temp["id"]),
        json!({}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let list = get_json(&gw, "/chat/api/threads").await;
    assert_eq!(list["threads"][0]["folder_id"], Value::Null);
}

/// A body, query or path the Chat API cannot read is refused in its own flat
/// `{code, message}` shape — never axum's plain text (review R1 item d).
#[tokio::test]
async fn unreadable_requests_are_flat_api_errors() {
    let (_state, gw) = gw().await;
    let f = folder(&gw, "F", json!({})).await;
    let fid = f["id"].as_i64().unwrap();
    let flat = |what: &'static str| {
        move |r: reqwest::Response| async move {
            let status = r.status();
            assert!(status.is_client_error(), "{what}: {status}");
            let text = r.text().await.unwrap();
            let v: Value =
                serde_json::from_str(&text).unwrap_or_else(|_| panic!("{what}: not JSON: {text}"));
            assert!(
                v["code"].is_string() && v["message"].is_string(),
                "{what}: {v}"
            );
            v
        }
    };
    // A missing field, a wrong type, not JSON at all.
    let v = flat("no threads choice")(
        post(&gw, &format!("/chat/api/folders/{fid}/delete"), json!({})).await,
    )
    .await;
    assert_eq!(v["code"], "bad_request");
    assert!(v["message"].as_str().unwrap().contains("threads"), "{v}");
    flat("wrong type")(post(&gw, "/chat/api/folders", json!({"name": 5})).await).await;
    let raw = gw
        .client()
        .post(format!("{gw}/chat/api/threads/1/move"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    flat("not JSON")(raw).await;
    let no_type = gw
        .client()
        .post(format!("{gw}/chat/api/folders"))
        .body("{}")
        .send()
        .await
        .unwrap();
    let v = flat("no content type")(no_type).await;
    assert_eq!(v["code"], "unsupported_media_type");
    // A path id that is not a number, on an action and an export.
    for (method, route) in [
        ("POST", "/chat/api/threads/abc/messages/1/delete"),
        ("GET", "/chat/api/threads/abc/export"),
        ("POST", "/chat/api/threads/abc/persist"),
        ("POST", "/chat/api/attachments/abc/mode"),
    ] {
        let r = match method {
            "GET" => gw.client().get(format!("{gw}{route}")).send().await,
            _ => {
                gw.client()
                    .post(format!("{gw}{route}"))
                    .json(&json!({"mode": "text"}))
                    .send()
                    .await
            }
        }
        .unwrap();
        flat("non-numeric id")(r).await;
    }
    // A query that does not parse.
    let r = gw
        .client()
        .get(format!(
            "{gw}/chat/api/folders/{fid}/export?format=md&archived=0&x"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "unknown query keys are ignored");
    let r = gw
        .client()
        .get(format!("{gw}/chat/api/search?q=abc&offset=x"))
        .send()
        .await
        .unwrap();
    flat("bad offset")(r).await;
}
