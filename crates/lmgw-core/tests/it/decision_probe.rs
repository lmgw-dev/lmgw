//! A decision row's container, asked as lmgw asks it (decision-models design
//! §3.6, §3.7): the load test's one `choice` question, and the start's
//! `GET /v1/models` read that says whether the image has the route. The
//! container is a wiremock server (`decision_world`).

use lmgw_api_types::ModelTest;
use lmgw_core::modelinfo;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::decision_world::{decision_world, healthy, ticket, DECISION, LLAMA_ANSWER};

/// llama.cpp's `/props` with the build it reports (Appendix A.2).
fn props(build: &str) -> Value {
    json!({"build_info": build, "total_slots": 4,
           "modalities": {"vision": false, "video": false, "audio": false},
           "default_generation_settings": {"n_ctx": 8192}})
}

/// The container's `GET /v1/models`: a build with the route lists
/// `decisions` (Appendix A.2), one from before it does not.
fn models(with_route: bool) -> Value {
    let mut laya = json!({"id": "laya", "object": "model", "owned_by": "llamacpp"});
    if with_route {
        laya["architecture"] =
            json!({"input_modalities": ["text"], "output_modalities": ["decisions"]});
    }
    json!({"object": "list", "data": [laya]})
}

/// The probe's answer as llama.cpp would give it.
fn answer(choice: &str, probabilities: [f64; 3]) -> Value {
    json!({"model": "laya",
           "answers": {"topic": {"type": "choice", "choice": choice,
               "probabilities": {"software": probabilities[0], "cooking": probabilities[1],
                                 "sports": probabilities[2]},
               "confidence": 0.97}},
           "usage": {"input_tokens": 36, "output_tokens": 0}})
}

/// A container with `/health`, `/props` at `build`, `/v1/models` and
/// `POST /v1/systemone` answering `systemone`.
async fn container(build: &str, with_route: bool, systemone: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    mount(&server, build, with_route, systemone).await;
    server
}

async fn mount(server: &MockServer, build: &str, with_route: bool, systemone: ResponseTemplate) {
    healthy(server).await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(props(build)))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models(with_route)))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(systemone)
        .mount(server)
        .await;
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

/// The warnings on the decision row's registry entry.
fn entry_warnings(state: &lmgw_core::state::SharedState) -> Vec<String> {
    state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == DECISION)
        .expect("the decision row's container is in the registry")
        .warnings
}

/// §3.7: the test asks one `choice` question through the row's own
/// container, its options in the order offered, and passes on an answer that
/// names one of them with probabilities summing to 1. The result shows the
/// input tokens and the answer; the model's choice is not judged.
#[tokio::test]
async fn the_decision_probe_asks_one_choice_question_and_shows_the_answer() {
    let server = container(
        "b11549-aa94f20",
        true,
        // Written out, in llama.cpp's order (not the alphabetical one a
        // `json!` object would give): software, cooking, sports.
        ResponseTemplate::new(200).set_body_raw(
            br#"{"model":"laya","answers":{"topic":{"type":"choice","choice":"cooking","probabilities":{"software":0.2,"cooking":0.7,"sports":0.1},"confidence":0.97}},"usage":{"input_tokens":36,"output_tokens":0}}"#.to_vec(),
            "application/json",
        ),
    )
    .await;
    let w = decision_world(&server.uri()).await;

    let passed = match modelinfo::local_model_test(&w.state, DECISION, Some("aux"))
        .await
        .unwrap()
    {
        ModelTest::Passed(p) => p,
        other => panic!("not a pass: {other:?}"),
    };
    assert_eq!(
        (passed.class.as_str(), passed.probe.as_str()),
        ("aux", "decide")
    );
    assert_eq!(passed.input_tokens, Some(36));
    let given = passed.answer.as_deref().expect("a choice answer");
    assert_eq!(given.choice, "cooking", "not judged");
    // llama.cpp's own order, kept through the JSON both dispatchers answer with.
    let wire = lmgw_core::ops::backends::to_json(Ok::<_, String>(&passed)).unwrap();
    assert_eq!(
        wire["answer"]["probabilities"],
        json!([
            {"option": "software", "probability": 0.2},
            {"option": "cooking", "probability": 0.7},
            {"option": "sports", "probability": 0.1},
        ]),
        "{wire}"
    );

    let sent = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.url.path() == "/v1/systemone")
        .expect("the question went to the container");
    let body = String::from_utf8(sent.body).unwrap();
    assert!(
        body.contains(r#""criteria":{"software":null,"cooking":null,"sports":null}"#),
        "the options keep their order: {body}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["model"],
        DECISION
    );
    assert!(
        entry_warnings(&w.state).is_empty(),
        "{:?}",
        entry_warnings(&w.state)
    );
}

/// An answer that does not answer the question fails, saying what was wrong:
/// probabilities that do not sum to 1, a choice that is none of the options.
#[tokio::test]
async fn the_decision_probe_fails_an_answer_that_is_not_one() {
    for (body, why) in [
        (answer("software", [0.5, 0.3, 0.1]), "sum to"),
        (answer("weather", [0.5, 0.3, 0.2]), "none of the options"),
        (
            json!({"model": "laya", "answers": {}, "usage": {"input_tokens": 3,
                "output_tokens": 0}}),
            "no answer under the question id 'topic'",
        ),
    ] {
        let server = container("b11549-aa94f20", true, ok(body)).await;
        let w = decision_world(&server.uri()).await;
        match modelinfo::local_model_test(&w.state, DECISION, Some("aux"))
            .await
            .unwrap()
        {
            ModelTest::Failed(f) => {
                assert_eq!(f.probe, "decide");
                assert!(f.error.contains(why), "{why}: {}", f.error);
            }
            other => panic!("{why}: not a failure: {other:?}"),
        }
    }
}

/// §3.6 at start: a container whose `GET /v1/models` lists no `decisions`
/// starts and serves anyway, with a warning on its entry naming the image
/// and its build. A local model test reads it again: on a build that lists
/// it the warning goes; a 404 on the route fails the test by name.
#[tokio::test]
async fn a_start_on_an_image_without_the_route_warns_and_the_test_reads_it_again() {
    let server = container(
        "b11226-0c6a6a7",
        false,
        ResponseTemplate::new(200).set_body_raw(LLAMA_ANSWER, "application/json"),
    )
    .await;
    let w = decision_world(&server.uri()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/systemone", w.gw))
        .header("content-type", "application/json")
        .body(ticket(&w.name(DECISION)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the start succeeded");
    let warnings = entry_warnings(&w.state).join("\n");
    assert!(
        warnings.contains("lists no decisions")
            && warnings.contains("llama.cpp:server-cuda")
            && warnings.contains("b11226-0c6a6a7"),
        "{warnings}"
    );

    // The same container, answering 404 on the route as an old build does:
    // the test fails naming the image, and the warning stays.
    server.reset().await;
    mount(
        &server,
        "b11226-0c6a6a7",
        false,
        ResponseTemplate::new(404).set_body_json(json!({"error": {"code": 404,
            "message": "File Not Found", "type": "not_found_error"}})),
    )
    .await;
    match modelinfo::local_model_test(&w.state, DECISION, Some("aux"))
        .await
        .unwrap()
    {
        ModelTest::Failed(f) => assert!(
            f.error.contains("HTTP 404")
                && f.error.contains("llama.cpp:server-cuda")
                && f.error.contains("b11361"),
            "{}",
            f.error
        ),
        other => panic!("not a failure: {other:?}"),
    }
    assert!(entry_warnings(&w.state)
        .join("\n")
        .contains("lists no decisions"));

    // Updated in place: the test's read clears the warning.
    server.reset().await;
    mount(
        &server,
        "b11549-aa94f20",
        true,
        ok(answer("software", [0.9, 0.05, 0.05])),
    )
    .await;
    assert!(matches!(
        modelinfo::local_model_test(&w.state, DECISION, Some("aux"))
            .await
            .unwrap(),
        ModelTest::Passed(_)
    ));
    assert!(
        entry_warnings(&w.state).is_empty(),
        "{:?}",
        entry_warnings(&w.state)
    );
}

/// A container that answers decisions and whose `GET /v1/models` answers
/// `listing`.
async fn container_listing(listing: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    healthy(&server).await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(props("b11549-aa94f20")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(listing)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(LLAMA_ANSWER, "application/json"))
        .mount(&server)
        .await;
    server
}

/// Ask the decision row once through the gateway, which starts it.
async fn ask(w: &crate::support::decision_world::DecisionWorld) -> u16 {
    reqwest::Client::new()
        .post(format!("{}/v1/systemone", w.gw))
        .header("content-type", "application/json")
        .body(ticket(&w.name(DECISION)))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// §3.6: a `GET /v1/models` that cannot be read at start — a body that is no
/// model list, an answer slower than what is left of the readiness budget —
/// is a warning saying so, and the start still succeeds.
#[tokio::test]
async fn an_unread_route_at_start_is_a_warning_not_a_failed_start() {
    for (listing, why) in [
        (
            ResponseTemplate::new(200).set_body_string("File Not Found"),
            "not a model list",
        ),
        (
            ResponseTemplate::new(200)
                .set_body_json(models(true))
                .set_delay(std::time::Duration::from_secs(3)),
            "could not be read",
        ),
    ] {
        let server = container_listing(listing).await;
        let w = decision_world(&server.uri()).await;
        assert_eq!(ask(&w).await, 200, "{why}: the start succeeded");
        let warnings = entry_warnings(&w.state).join("\n");
        assert!(
            warnings.contains("GET /v1/models could not be read") && warnings.contains(why),
            "{why}: {warnings}"
        );
    }
}

/// A read that fails never replaces an answer read before (as `/props`'s
/// refresh keeps facts): not "listed", not "missing". An answer replaces
/// either, and a stale generation finds nothing.
#[tokio::test]
async fn a_failed_route_read_never_replaces_an_answer() {
    use lmgw_core::runtime::registry::{DecisionRoute, RouteRefresh};
    use lmgw_core::runtime::Class;

    let server = container_listing(ResponseTemplate::new(200).set_body_json(models(true))).await;
    let w = decision_world(&server.uri()).await;
    assert_eq!(ask(&w).await, 200);
    let generation = w
        .state
        .runtime()
        .list()
        .into_iter()
        .find(|v| v.model_id == DECISION)
        .unwrap()
        .generation;
    let rt = w.state.runtime();
    let refresh = |route| rt.refresh_decision_route(Class::Aux, DECISION, generation, route);
    let unread = || DecisionRoute::Unread("GET /v1/models could not be read (busy)".into());

    assert_eq!(refresh(unread()), RouteRefresh::Kept, "listed stays");
    assert!(entry_warnings(&w.state).is_empty());
    let missing = DecisionRoute::Missing("the image lists no decisions".into());
    assert_eq!(refresh(missing), RouteRefresh::Stored);
    assert_eq!(entry_warnings(&w.state), ["the image lists no decisions"]);
    assert_eq!(refresh(unread()), RouteRefresh::Kept, "missing stays");
    assert_eq!(entry_warnings(&w.state), ["the image lists no decisions"]);
    assert_eq!(refresh(DecisionRoute::Listed), RouteRefresh::Stored);
    assert!(entry_warnings(&w.state).is_empty());
    assert_eq!(
        rt.refresh_decision_route(Class::Aux, DECISION, generation + 1, DecisionRoute::Listed),
        RouteRefresh::Gone
    );
}

/// §3.6 at adoption: a decision container a previous lmgw started is read
/// again after its probe, as `/props` is, and keeps its warning when its
/// `GET /v1/models` lists no decisions (`gpu_world`'s containers list none).
#[tokio::test]
async fn adoption_reads_the_route_again() {
    use crate::support::gpu_world::{Gpu, GIB};

    let gpu = Gpu::new(24 * GIB, 2, 5).await;
    gpu.aux("laya", lmgw_core::config::AuxKind::Decision, GIB)
        .await;
    gpu.world()
        .props
        .insert("laya".into(), props("b11226-0c6a6a7"));
    crate::common::container_wire(
        lmgw_core::ops::container(&gpu.state, None, Some("laya"), "start", false, None)
            .await
            .expect("the model starts"),
    );
    gpu.restart();
    lmgw_core::runtime::lifecycle::boot(&gpu.state).await;

    let view = gpu.state.runtime().list();
    assert_eq!(view.len(), 1, "adopted: {view:?}");
    assert_eq!(view[0].state.as_str(), "ready");
    assert_eq!(gpu.runs(), ["laya"], "adopted, not started again");
    assert_eq!(view[0].warnings.len(), 1, "{:?}", view[0].warnings);
    let w = &view[0].warnings[0];
    assert!(
        w.contains("lists no decisions") && w.contains("b11226-0c6a6a7"),
        "{w}"
    );
}
