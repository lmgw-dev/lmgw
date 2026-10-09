//! What a device's tightening is measured against (client-apps design
//! §6.6): the server an entry names, not its label string; the tool's own
//! name in either spelling; the owner's floor, which a device removing an
//! entry, moving a thread or deleting its folder never lowers; and the
//! malformed `require_approval` shapes refused for every writer.

use serde_json::{json, Value};

use super::{chat_thread, device_world, get, pair, post};
use crate::realtime_chat_thread::World;

fn status_code(s: u16, v: &Value, want: (u16, &str), naming: &[&str]) {
    assert_eq!((s, v["code"].as_str()), (want.0, Some(want.1)), "{v}");
    let m = v["message"].as_str().unwrap();
    for n in naming {
        assert!(m.contains(n), "names {n}: {v}");
    }
}

fn refused(s: u16, v: &Value, naming: &[&str]) {
    status_code(s, v, (403, "approval_loosen_refused"), naming);
}

fn bad(s: u16, v: &Value, naming: &[&str]) {
    status_code(s, v, (400, "bad_request"), naming);
}

fn entry(label: &str, ra: Value) -> Value {
    json!({ "server_label": label, "require_approval": ra })
}

fn tools(entries: &[Value]) -> Value {
    json!({ "mcp_tools": entries })
}

fn docs(ra: Value) -> Value {
    tools(&[entry("docs", ra)])
}

fn always(names: &[&str]) -> Value {
    json!({ "always": { "tool_names": names } })
}

/// A registered server named `github` with the tool prefix `gh`, switched
/// off (nothing connects): both labels name it.
async fn github(w: &World) {
    sqlx::query(
        "INSERT INTO mcp_servers (name, enabled, transport, url, tool_prefix) \
         VALUES ('github', 0, 'http', 'http://127.0.0.1:9/mcp', 'gh')",
    )
    .execute(&w.state.db)
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
}

async fn thread_tools(w: &World, tid: i64) -> Value {
    let (_, t) = get(w, &w.gw.client(), &format!("/chat/api/threads/{tid}")).await;
    t["thread"]["mcp_tools"].clone()
}

/// A thread's settings path.
fn settings(tid: i64) -> String {
    format!("/chat/api/threads/{tid}/settings")
}

/// Created in folder `fid` by `client`: the thread's id.
async fn thread_in(w: &World, client: &reqwest::Client, fid: i64) -> i64 {
    let (s, t) = post(
        w,
        client,
        "/chat/api/threads",
        json!({ "model_alias": "chatty", "folder_id": fid }),
    )
    .await;
    assert_eq!(s, 200, "{t}");
    t["id"].as_i64().unwrap()
}

/// A folder made by `client` with `mcp_tools` defaults: its id.
async fn folder(w: &World, client: &reqwest::Client, mcp_tools: Value) -> i64 {
    let (s, f) = post(
        w,
        client,
        "/chat/api/folders",
        json!({ "name": "F", "defaults": { "model_alias": "chatty", "mcp_tools": mcp_tools } }),
    )
    .await;
    assert_eq!(s, 200, "{f}");
    f["id"].as_i64().unwrap()
}

async fn move_to(w: &World, client: &reqwest::Client, tid: i64, fid: Option<i64>) {
    let (s, v) = post(
        w,
        client,
        &format!("/chat/api/threads/{tid}/move"),
        json!({ "folder_id": fid }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

/// A server is one target by its prefix and by its name: another label for
/// it is compared with the rule the first one carried.
#[tokio::test]
async fn a_server_s_name_is_the_same_target_as_its_prefix() {
    let (w, d) = device_world().await;
    github(&w).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let path = settings(tid);
    let (s, v) = post(&w, &owner, &path, tools(&[entry("gh", json!("always"))])).await;
    assert_eq!(s, 200, "{v}");

    let (s, v) = post(
        &w,
        &d.client,
        &path,
        tools(&[entry("github", json!("never"))]),
    )
    .await;
    refused(s, &v, &["'github'"]);
    assert_eq!(thread_tools(&w, tid).await[0]["server_label"], "gh");
    // Renamed at the same rule: no loosening.
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        tools(&[entry("github", json!("always"))]),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    // And the owner's floor is the server's, by either label.
    let (s, v) = post(&w, &d.client, &path, tools(&[])).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, tools(&[entry("gh", json!("never"))])).await;
    refused(s, &v, &["'gh'", "what the owner last set"]);
}

/// Two entries for one server are refused for every writer: a turn keeps
/// one of them, and which one is not what the list shows.
#[tokio::test]
async fn two_entries_for_one_server_are_a_400_for_everyone() {
    let (w, d) = device_world().await;
    github(&w).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let path = settings(tid);
    let (s, v) = post(&w, &owner, &path, tools(&[entry("gh", json!("always"))])).await;
    assert_eq!(s, 200, "{v}");
    let both = tools(&[
        entry("gh", json!("always")),
        entry("github", json!("never")),
    ]);
    for client in [&d.client, &owner] {
        let (s, v) = post(&w, client, &path, both.clone()).await;
        bad(s, &v, &["'gh' and 'github' name the same server"]);
        let twice = tools(&[
            entry("docs", json!("always")),
            entry(" docs", json!("never")),
        ]);
        let (s, v) = post(&w, client, &path, twice).await;
        bad(s, &v, &["'docs' is listed twice"]);
    }
    assert_eq!(
        thread_tools(&w, tid).await,
        json!([{ "server_label": "gh", "allowed_tools": null, "require_approval": "always" }])
    );
    // The same for a folder's defaults.
    let (s, v) = post(
        &w,
        &d.client,
        "/chat/api/folders",
        json!({ "name": "F", "defaults": both }),
    )
    .await;
    bad(s, &v, &["the same server"]);
}

/// A tool named in either spelling is one tool: `gh__search` is `search`
/// on the `gh` server, as a turn matches it.
#[tokio::test]
async fn either_spelling_of_a_tool_is_the_same_tool() {
    let (w, d) = device_world().await;
    github(&w).await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let path = settings(tid);
    let set = |ra: Value| tools(&[entry("gh", ra)]);

    let (s, v) = post(&w, &owner, &path, set(always(&["search"]))).await;
    assert_eq!(s, 200, "{v}");
    let ungate = json!({ "always": { "tool_names": ["search"] },
                         "never": { "tool_names": ["gh__search"] } });
    let (s, v) = post(&w, &d.client, &path, set(ungate)).await;
    refused(s, &v, &["'gh'", "'search'"]);

    // The other way round: the prefixed spelling the owner wrote is the
    // same tool as the plain one a device writes.
    let (s, v) = post(&w, &owner, &path, set(always(&["gh__a"]))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, set(always(&["a"]))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, set(always(&["gh__a", "b"]))).await;
    assert_eq!(s, 200, "{v}");
    let ungate = json!({ "never": { "tool_names": ["gh__a"] } });
    let (s, v) = post(&w, &d.client, &path, set(ungate)).await;
    refused(s, &v, &["'a'"]);
}

/// The owner's floor: a device removing an entry leaves it; the owner
/// removing one, or writing a looser rule, moves it.
#[tokio::test]
async fn only_the_owner_moves_a_thread_s_floor() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let path = settings(tid);
    let (s, v) = post(&w, &owner, &path, docs(json!("always"))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, tools(&[])).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, docs(json!("never"))).await;
    refused(s, &v, &["'docs'", "what the owner last set"]);
    let (s, v) = post(&w, &d.client, &path, docs(always(&["a"]))).await;
    refused(s, &v, &["other tools"]);
    let (s, v) = post(&w, &d.client, &path, docs(json!("always"))).await;
    assert_eq!(s, 200, "{v}");

    // The owner removes the entry: its floor goes with it.
    let (s, v) = post(&w, &owner, &path, tools(&[])).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, docs(json!("never"))).await;
    assert_eq!(s, 200, "{v}");
    // The owner's write of the entry is its floor, a looser one too.
    let (s, v) = post(&w, &owner, &path, docs(always(&["a"]))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, tools(&[])).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, docs(json!("never"))).await;
    refused(s, &v, &["'a'"]);
    let (s, v) = post(&w, &d.client, &path, docs(always(&["a", "b"]))).await;
    assert_eq!(s, 200, "{v}");
}

/// A folder's defaults keep the owner's floor the same way.
#[tokio::test]
async fn a_folder_s_floor_survives_a_device_s_removal() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let fid = folder(&w, &owner, json!([entry("docs", json!("always"))])).await;
    let path = format!("/chat/api/folders/{fid}");
    let set = |mcp_tools: Value| json!({ "defaults": { "model_alias": "chatty", "mcp_tools": mcp_tools } });
    let (s, v) = post(&w, &d.client, &path, set(json!([]))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        set(json!([entry("docs", json!("never"))])),
    )
    .await;
    refused(s, &v, &["'docs'", "what the owner last set"]);
    // `defaults_patch` is a write of the same field.
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        json!({ "defaults_patch": { "mcp_tools": [entry("docs", json!("never"))] } }),
    )
    .await;
    refused(s, &v, &["'docs'"]);
    // An owner's save that does not name the entry leaves its floor; one
    // that does moves it.
    let (s, v) = post(&w, &owner, &path, set(json!([]))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        set(json!([entry("docs", json!("never"))])),
    )
    .await;
    refused(s, &v, &["'docs'"]);
    let (s, v) = post(
        &w,
        &owner,
        &path,
        set(json!([entry("docs", json!("never"))])),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, set(json!([]))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(
        &w,
        &d.client,
        &path,
        set(json!([entry("docs", json!("never"))])),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

/// A thread made in a folder starts from the folder's floor, and leaving
/// the folder — a move out, a move into a device's own folder, the folder's
/// delete — never lowers it.
#[tokio::test]
async fn moving_a_thread_never_lowers_its_floor() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let strict = folder(&w, &owner, json!([entry("docs", json!("always"))])).await;
    let tid = thread_in(&w, &d.client, strict).await;
    let path = settings(tid);
    assert_eq!(thread_tools(&w, tid).await[0]["require_approval"], "always");
    let (s, v) = post(&w, &d.client, &path, tools(&[])).await;
    assert_eq!(s, 200, "{v}");

    move_to(&w, &d.client, tid, None).await;
    let (s, v) = post(&w, &d.client, &path, docs(json!("never"))).await;
    refused(s, &v, &["'docs'", "what the owner last set"]);

    let own = folder(&w, &d.client, json!([entry("docs", json!("never"))])).await;
    move_to(&w, &d.client, tid, Some(own)).await;
    let (s, v) = post(&w, &d.client, &path, docs(json!("never"))).await;
    refused(s, &v, &["'docs'"]);
    let (s, v) = post(&w, &d.client, &path, docs(json!("always"))).await;
    assert_eq!(s, 200, "{v}");
}

/// A folder default the owner set after the thread was made counts while
/// the thread is in the folder, and goes with it when it leaves — by a
/// move or by the folder's delete.
#[tokio::test]
async fn a_thread_leaving_its_folder_keeps_the_folder_s_rules() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    for by_delete in [false, true] {
        let fid = folder(&w, &owner, json!([])).await;
        let tid = thread_in(&w, &d.client, fid).await;
        let (s, v) = post(
            &w,
            &owner,
            &format!("/chat/api/folders/{fid}"),
            json!({ "defaults": { "model_alias": "chatty",
                    "mcp_tools": [entry("docs", json!("always"))] } }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        let path = settings(tid);
        let (s, v) = post(&w, &d.client, &path, docs(json!("never"))).await;
        refused(s, &v, &["'docs'", "the folder's default"]);

        if by_delete {
            let (s, v) = post(
                &w,
                &d.client,
                &format!("/chat/api/folders/{fid}/delete"),
                json!({ "threads": "keep" }),
            )
            .await;
            assert_eq!(s, 200, "{v}");
        } else {
            move_to(&w, &d.client, tid, None).await;
        }
        let (s, v) = post(&w, &d.client, &path, docs(json!("never"))).await;
        refused(s, &v, &["'docs'"]);
        let (s, v) = post(&w, &d.client, &path, docs(json!("always"))).await;
        assert_eq!(s, 200, "{v}");
    }
}

/// A label out of a scoped device's reach is answered as such, before
/// whether its rule loosens.
#[tokio::test]
async fn an_out_of_scope_label_is_refused_for_its_scope_first() {
    let (w, _) = device_world().await;
    let owner = w.gw.client();
    let d = pair(
        &w,
        "watch",
        json!({ "tool_scope_mode": "allow", "tool_scope_patterns": "docs__*" }),
    )
    .await;
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let path = settings(tid);
    let (s, v) = post(&w, &owner, &path, tools(&[entry("kb", json!("always"))])).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = post(&w, &d.client, &path, tools(&[entry("kb", json!("never"))])).await;
    status_code(s, &v, (403, "tool_label_out_of_scope"), &["'kb'"]);
}

/// A `require_approval` the reader cannot take as written is a 400 for
/// every writer, never read as "gates nothing".
#[tokio::test]
async fn a_malformed_require_approval_is_refused_for_everyone() {
    let (w, d) = device_world().await;
    let owner = w.gw.client();
    let tid = chat_thread(&w, &d.client, "chatty").await;
    let path = settings(tid);
    for (ra, says) in [
        (json!({ "always": ["a"] }), "got array"),
        (json!({ "always": {} }), "names no tools"),
        (
            json!({ "alwyas": { "tool_names": ["a"] } }),
            "unknown key 'alwyas'",
        ),
        (
            json!({ "never": { "toolnames": ["a"] } }),
            "unknown key 'toolnames'",
        ),
    ] {
        for client in [&owner, &d.client] {
            let (s, v) = post(&w, client, &path, docs(ra.clone())).await;
            bad(s, &v, &["require_approval", says]);
        }
    }
    assert_eq!(thread_tools(&w, tid).await, json!([]));
}
