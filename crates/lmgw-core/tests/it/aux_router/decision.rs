//! The decision kind's gate, both ways (decision-models design §2.2): the
//! generation, embedding and rerank routes refuse a decision row by name, so
//! do realtime's model resolution, the in-process chat callers and the audio
//! routes (which refuse every aux row), `/v1/systemone` refuses the other aux
//! kinds with 501, and the token counters keep answering on a decision row —
//! all before anything starts that could not answer.

use lmgw_core::gate::RouteCheck;
use lmgw_core::realtime::resolve::{is_chat_alias, resolve_chat};
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::decision_world::{decision_world, healthy, DECISION, EMBED, RERANK};

/// A dead port: a request that got as far as a start would fail there after
/// the world's two-second load timeout, and say so in the podman's verbs.
const DEAD: &str = "http://127.0.0.1:1";

async fn post(base: &str, route: &str, body: Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{base}{route}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// A decision model cannot generate, embed or rerank: llama-server answers
/// chat with a 500, embeddings with a 400 and rerank with a 501 (Appendix
/// A.7). Each route refuses the row by name, pointing at `/v1/systemone`,
/// with its own status for a model it does not serve — and before
/// admission, so no container starts for a request it cannot answer.
#[tokio::test]
async fn a_decision_row_is_refused_on_the_generation_embedding_and_rerank_routes() {
    let w = decision_world(DEAD).await;
    let laya = w.name(DECISION);
    let user = json!([{"role": "user", "content": "hi"}]);
    for (route, body) in [
        (
            "/v1/chat/completions",
            json!({"model": laya, "messages": user}),
        ),
        (
            "/v1/messages",
            json!({"model": laya, "max_tokens": 8, "messages": user}),
        ),
        ("/v1/responses", json!({"model": laya, "input": "hi"})),
        ("/v1/completions", json!({"model": laya, "prompt": "hi"})),
        ("/v1/embeddings", json!({"model": laya, "input": "hi"})),
        (
            "/v1/rerank",
            json!({"model": laya, "query": "q", "documents": ["a"]}),
        ),
    ] {
        let (status, body) = post(&w.gw.to_string(), route, body).await;
        assert_eq!(status, 400, "{route}: {body}");
        let msg = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains(DECISION) && msg.contains("/v1/systemone") && msg.contains(route),
            "{route}: the refusal names the model, the route and where to send it: {body}"
        );
    }
    assert_eq!(w.podman.runs(), 0, "no container starts for a refusal");
}

/// The mirror (§2.2): `/v1/systemone` refuses an embedder and a reranker by
/// name and kind with llama.cpp's own status for the refusal, 501
/// `not_supported_error`, in llama.cpp's error shape — and logs it under the
/// route's own label, with the row it named.
#[tokio::test]
async fn systemone_refuses_an_embedding_or_rerank_row_with_501() {
    let w = decision_world(DEAD).await;
    for (id, kind) in [(EMBED, "embed"), (RERANK, "rerank")] {
        let (status, body) = post(
            &w.gw.to_string(),
            "/v1/systemone",
            json!({"model": w.name(id), "state": "hi",
                   "questions": {"q": {"type": "noul", "instructions": "Is it?"}}}),
        )
        .await;
        assert_eq!(status, 501, "{id}: {body}");
        assert_eq!(body["error"]["code"], 501, "{body}");
        assert_eq!(body["error"]["type"], "not_supported_error", "{body}");
        assert_eq!(body["error"]["lmgw_code"], "not_supported", "{body}");
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains(id) && msg.contains(kind) && msg.contains("not a decision model"),
            "{msg}"
        );
    }
    assert_eq!(w.podman.runs(), 0, "no container starts for a refusal");

    let rows = store::query_logs(&w.state.db, &Default::default())
        .await
        .unwrap();
    for id in [EMBED, RERANK] {
        let row = rows
            .iter()
            .find(|r| r.requested_alias == w.name(id))
            .unwrap_or_else(|| panic!("no row for {id}: {rows:?}"));
        assert_eq!(
            (row.status, row.ingress_proto.as_str(), row.class.as_deref()),
            (501, "systemone", Some("aux")),
            "{row:?}"
        );
        assert_eq!(row.upstream_model.as_deref(), Some(id), "{row:?}");
        assert_eq!(row.error_kind.as_deref(), Some("not_supported"), "{row:?}");
    }
}

/// The counters stay open (§2.2): `/tokenize` works on a decision model and
/// is how a client sizes a state against its ubatch, and both count routes
/// answer from the same server.
#[tokio::test]
async fn the_token_counters_still_answer_on_a_decision_row() {
    let container = MockServer::start().await;
    healthy(&container).await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens": [25521, 1533]})))
        .mount(&container)
        .await;
    let w = decision_world(&container.uri()).await;
    let laya = w.name(DECISION);
    let base = w.gw.to_string();

    let (status, body) = post(
        &base,
        "/tokenize",
        json!({"model": laya, "content": "hi there"}),
    )
    .await;
    assert_eq!((status, &body), (200, &json!({"tokens": [25521, 1533]})));

    let (status, body) = post(
        &base,
        "/v1/count_tokens",
        json!({"model": laya, "input": "hi there"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tokens"], 2, "{body}");

    let (status, body) = post(
        &base,
        "/v1/messages/count_tokens",
        json!({"model": laya, "messages": [{"role": "user", "content": "hi there"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["input_tokens"], 2, "{body}");
}

/// A realtime session's model is a chat alias as `/v1/chat/completions`
/// takes one, so a decision row is none: not as the name a client sends, and
/// not as a setting that names it — refused at resolution, before the
/// background warm could start its container for a session that would refuse
/// every turn.
#[tokio::test]
async fn realtime_never_takes_a_decision_row_as_its_chat_model() {
    let w = decision_world(DEAD).await;
    let laya = w.name(DECISION);
    let snap = w.state.snapshot();
    assert!(!is_chat_alias(&snap, &laya));
    let e = resolve_chat(&snap, Some(&laya)).unwrap_err();
    assert_eq!(e.kind(), "unknown_alias", "{e}");

    let mut s = snap.settings.clone();
    s.realtime.model_map.insert("voice".into(), laya.clone());
    store::save_settings(&w.state.db, &s).await.unwrap();
    let snap = w.state.reload_snapshot().await.unwrap();
    let e = resolve_chat(&snap, Some("voice")).unwrap_err();
    assert!(e.to_string().contains("realtime.model_map"), "{e}");
    assert_eq!(w.podman.runs(), 0);
}

/// The in-process chat callers — the dashboard's chats, agent runs, MCP
/// sampling, a profile's Try — open the gate with `RouteCheck::None`, which
/// runs the decision guard too: a decision row a setting names is refused by
/// name, saying where it does answer, before its container starts.
#[tokio::test]
async fn the_in_process_callers_refuse_a_decision_row_before_it_starts() {
    let w = decision_world(DEAD).await;
    let laya = w.name(DECISION);
    let f = lmgw_core::gate::open(&w.state, &laya, RouteCheck::None)
        .await
        .unwrap_err();
    assert_eq!(f.error.kind(), "unsupported", "{}", f.error);
    let msg = f.error.to_string();
    assert!(
        msg.contains(DECISION) && msg.contains("/v1/systemone") && msg.contains("cannot generate"),
        "{msg}"
    );
    assert!(f.route.is_some(), "the refusal names the row it is about");
    assert_eq!(w.podman.runs(), 0, "no container starts for a refusal");
}

/// The audio routes check the protocol, and an aux row speaks llama_cpp, so
/// they used to start an embedder's, a reranker's or a decision model's
/// container for audio it has no route for. Each is refused by name now.
#[tokio::test]
async fn the_audio_routes_refuse_every_aux_row() {
    let w = decision_world(DEAD).await;
    for id in [DECISION, EMBED, RERANK] {
        let (status, body) = post(
            &w.gw.to_string(),
            "/v1/audio/speech",
            json!({"model": w.name(id), "input": "hello"}),
        )
        .await;
        assert_eq!(status, 400, "{id}: {body}");
        let msg = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains(id) && msg.contains("aux model"),
            "{id}: {body}"
        );
    }
    assert_eq!(w.podman.runs(), 0, "no container starts for a refusal");
}
