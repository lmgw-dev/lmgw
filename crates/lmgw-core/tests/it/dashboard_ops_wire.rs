//! What the dashboard's own routing and conversation ops answer over HTTP
//! (`POST /api/op/<name>`): `response_chain_delete` and `responses_gc` say what
//! they did, `upstream_test` says it connected, `upstream_set_full` and
//! `alias_set` name the row they wrote, and `model_visibility` says how many
//! models it named. Each answer reads back through the type the document
//! describes and keeps the keys it has always sent.

use lmgw_api_types as dto;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewUpstream, StoredResponse};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, Gw};

/// `POST /api/op/<name>` → `(status, body)`.
async fn post(gw: &Gw, name: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

/// The answer of an op that succeeded.
async fn op(gw: &Gw, name: &str, body: Value) -> Value {
    let (status, live) = post(gw, name, body).await;
    assert_eq!(status, 200, "{name}: {live}");
    live
}

/// The message of an op that was refused.
async fn refused(gw: &Gw, name: &str, body: Value) -> String {
    let (status, live) = post(gw, name, body).await;
    assert_eq!(status, 400, "{name}: {live}");
    assert_eq!(live["code"], "op_failed", "{live}");
    live["message"].as_str().unwrap().to_string()
}

/// The keys of an answer, sorted: the set it sends and no other.
fn keys(live: &Value) -> Vec<&str> {
    let mut k: Vec<&str> = live
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    k.sort_unstable();
    k
}

async fn gateway() -> (SharedState, Gw) {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = common::serve(state.clone()).await;
    (state, gw)
}

/// `n` stored responses, each in a chain of its own named `<prefix><i>`, the
/// oldest first.
async fn seed(state: &SharedState, prefix: &str, n: usize, created: &str) {
    for i in 0..n {
        let id = format!("{prefix}{i}");
        store::insert_response(
            &state.db,
            &StoredResponse {
                id: id.clone(),
                chain_id: id.clone(),
                previous_response_id: None,
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
        sqlx::query("UPDATE responses SET created_at = ?1 WHERE id = ?2")
            .bind(format!("{created} 00:00:{i:02}"))
            .bind(&id)
            .execute(&state.db)
            .await
            .unwrap();
    }
}

async fn upstream(state: &SharedState, name: &str, base_url: String) -> i64 {
    let id = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: name.into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url,
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: true,
            expose_prefix: "cat".into(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    id
}

/// A delete and both garbage collections answer `{ok, message}`: the message
/// counts what went.
#[tokio::test]
async fn the_response_ops_answer_a_message() {
    let (state, gw) = gateway().await;

    let live = op(&gw, "responses_gc", json!({})).await;
    let none = common::round_trips::<dto::MessageAck>("responses_gc, empty", &live);
    assert_eq!(live, json!({"ok": true, "message": "nothing to evict"}));
    assert!(none.ok);

    // Three conversations; the store keeps two by the rules, the oldest goes.
    seed(&state, "old", 3, "2020-01-01").await;
    op(
        &gw,
        "settings_set_full",
        json!({"responses_max_chains": 2, "responses_retention_hours": 0}),
    )
    .await;
    let live = op(&gw, "responses_gc", json!({"scope": "rules"})).await;
    let evicted = common::round_trips::<dto::MessageAck>("responses_gc, rules", &live);
    assert_eq!(evicted.message, "evicted 1 response(s)");
    assert_eq!(keys(&live), ["message", "ok"]);

    let live = op(&gw, "response_chain_delete", json!({"chain_id": "old1"})).await;
    let deleted = common::round_trips::<dto::MessageAck>("response_chain_delete", &live);
    assert_eq!(deleted.message, "deleted 1 response(s)");
    assert_eq!(keys(&live), ["message", "ok"]);
    // A chain that is not there is nothing deleted, not a refusal.
    let live = op(&gw, "response_chain_delete", json!({"chain_id": "old1"})).await;
    assert_eq!(live["message"], "deleted 0 response(s)", "{live}");

    let live = op(&gw, "responses_gc", json!({"scope": "all"})).await;
    let cleared = common::round_trips::<dto::MessageAck>("responses_gc, all", &live);
    assert_eq!(cleared.message, "cleared 1 stored response(s)");
    assert_eq!(store::count_responses(&state.db).await.unwrap(), (0, 0));

    assert_eq!(
        refused(&gw, "response_chain_delete", json!({})).await,
        "pass chain_id"
    );
}

/// A connection that works answers `{ok}`; one that
/// does not is the usual refusal, with the upstream's own words.
#[tokio::test]
async fn an_upstream_test_answers_that_it_connected() {
    let (state, gw) = gateway().await;
    let up = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&up)
        .await;
    let id = upstream(&state, "reachable", up.uri()).await;

    let live = op(&gw, "upstream_test", json!({"id": id})).await;
    let tested = common::round_trips::<dto::UpstreamTested>("upstream_test", &live);
    assert_eq!(live, json!({"ok": true}));
    assert!(tested.ok);

    let down = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("no key"))
        .mount(&down)
        .await;
    let id = upstream(&state, "refusing", down.uri()).await;
    assert_eq!(
        refused(&gw, "upstream_test", json!({"id": id})).await,
        "connection test failed — 401 Unauthorized: no key"
    );
    assert_eq!(refused(&gw, "upstream_test", json!({})).await, "pass id");
    assert_eq!(
        refused(&gw, "upstream_test", json!({"id": 999})).await,
        "no upstream with id 999"
    );
}

/// An upstream and an alias are written with `{ok, id, message}`: the new row's
/// id on a create, the row's own on an update.
#[tokio::test]
async fn an_upstream_or_alias_save_names_the_row() {
    let (_state, gw) = gateway().await;
    let base = "http://127.0.0.1:9/v1";

    let live = op(
        &gw,
        "upstream_set_full",
        json!({"action": "create", "name": "cloud", "base_url": base}),
    )
    .await;
    let made = common::round_trips::<dto::RowSaved>("upstream_set_full, create", &live);
    assert_eq!(keys(&live), ["id", "message", "ok"]);
    assert!(made.ok && made.id > 0);
    assert_eq!(made.message, "upstream 'cloud' created");

    let live = op(
        &gw,
        "upstream_set_full",
        json!({"action": "update", "id": made.id, "timeout_ms": 20_000}),
    )
    .await;
    let again = common::round_trips::<dto::RowSaved>("upstream_set_full, update", &live);
    assert_eq!(
        (again.id, again.message.as_str()),
        (made.id, "upstream 'cloud' updated")
    );

    // What the save settled is said after the sentence.
    let live = op(
        &gw,
        "upstream_set_full",
        json!({"action": "create", "name": "local", "base_url": base, "protocol": "llama_cpp"}),
    )
    .await;
    let local = common::round_trips::<dto::RowSaved>("upstream_set_full, settled", &live);
    assert_eq!(
        local.message,
        "upstream 'local' created (kind set to llama_server, the only kind protocol llama_cpp \
         goes with)"
    );
    assert_eq!(
        refused(&gw, "upstream_set_full", json!({"action": "update"})).await,
        "pass id"
    );

    let live = op(
        &gw,
        "alias_set",
        json!({"action": "create", "alias": "fast", "upstream_id": made.id,
               "upstream_model": "gpt-x", "overrides": {"temperature": 0.5}}),
    )
    .await;
    let alias = common::round_trips::<dto::RowSaved>("alias_set, create", &live);
    assert_eq!(keys(&live), ["id", "message", "ok"]);
    assert_eq!(alias.message, "alias 'fast' created");

    let live = op(
        &gw,
        "alias_set",
        json!({"action": "update", "id": alias.id, "enabled": false}),
    )
    .await;
    let again = common::round_trips::<dto::RowSaved>("alias_set, update", &live);
    assert_eq!(
        (again.id, again.message.as_str()),
        (alias.id, "alias 'fast' updated")
    );
    assert_eq!(
        refused(&gw, "alias_set", json!({"action": "update", "id": 999})).await,
        "no alias with id 999"
    );
}

/// Hiding or restoring models says which, and how many distinct ones the call
/// named: a repeat, a blank and the same id in both fields count once.
#[tokio::test]
async fn model_visibility_answers_how_many_it_named() {
    let (state, gw) = gateway().await;
    let up = MockServer::start().await;
    let id = upstream(&state, "catalog", up.uri()).await;
    let call = |action: &str, extra: Value| {
        let mut body = json!({"action": action, "upstream_id": id});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        body
    };

    let live = op(
        &gw,
        "model_visibility",
        call("hide", json!({"model_id": "a"})),
    )
    .await;
    let one = common::round_trips::<dto::ModelVisibilitySaved>("hide one", &live);
    assert_eq!(keys(&live), ["count", "message", "ok"]);
    assert!(live["count"].is_u64(), "{live}");
    assert_eq!(
        (one.count, one.message.as_str()),
        (1, "'a' hidden from the client-facing model list")
    );

    let live = op(
        &gw,
        "model_visibility",
        call(
            "hide",
            json!({"model_id": "a", "model_ids": ["b", " b ", "", "c"]}),
        ),
    )
    .await;
    let three = common::round_trips::<dto::ModelVisibilitySaved>("hide three", &live);
    assert_eq!(
        (three.count, three.message.as_str()),
        (3, "3 models hidden from the client-facing model list")
    );

    let live = op(
        &gw,
        "model_visibility",
        call("unhide", json!({"model_ids": ["c"]})),
    )
    .await;
    let back = common::round_trips::<dto::ModelVisibilitySaved>("unhide one", &live);
    assert_eq!(
        (back.count, back.message.as_str()),
        (1, "'c' restored to the client-facing model list")
    );

    assert_eq!(
        refused(&gw, "model_visibility", call("hide", json!({}))).await,
        "pass model_id or model_ids"
    );
    assert_eq!(
        refused(
            &gw,
            "model_visibility",
            call("nope", json!({"model_id": "a"}))
        )
        .await,
        "unknown action 'nope' (hide, unhide)"
    );
}

/// The document names the type each of the six ops answers, and no op is
/// documented as the generic `OpOutcome` any more.
#[tokio::test]
async fn the_document_names_the_answer_of_each_op() {
    let doc = lmgw_core::openapi::admin_doc();
    for (name, want) in [
        ("response_chain_delete", "MessageAck"),
        ("responses_gc", "MessageAck"),
        ("upstream_test", "UpstreamTested"),
        ("upstream_set_full", "RowSaved"),
        ("alias_set", "RowSaved"),
        ("model_visibility", "ModelVisibilitySaved"),
    ] {
        let schema = &doc["paths"][format!("/api/op/{name}")]["post"]["responses"]["200"]
            ["content"]["application/json"]["schema"];
        let sent = schema["$ref"]
            .as_str()
            .and_then(|r| r.rsplit('/').next())
            .or_else(|| schema["title"].as_str());
        assert_eq!(sent, Some(want), "{name}: {schema}");
    }
    assert!(
        !doc.to_string().contains("OpOutcome"),
        "an op is documented as the generic OpOutcome"
    );

    // What every one of them sends is required.
    let required = |name: &str| -> Vec<String> {
        let schemas = common::documented_schemas(doc, name);
        let mut r: Vec<String> = schemas
            .first()
            .unwrap_or_else(|| panic!("the document has no {name}"))["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} has no required list"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        r.sort();
        r
    };
    assert_eq!(required("UpstreamTested"), ["ok"]);
    assert_eq!(required("ModelVisibilitySaved"), ["count", "message", "ok"]);
}
