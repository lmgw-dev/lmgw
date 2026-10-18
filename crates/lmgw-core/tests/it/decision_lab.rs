//! The Decision lab's server (labs-store design §2) on `decision_world`'s
//! wiremock decision container: the model list with a non-causal row's
//! facts and limits; cells dispatched through the real `/v1/systemone`
//! handler (the container receives exactly the builder's bytes with `model`
//! rewritten, the request log has its `systemone` row, the run keeps request
//! and response verbatim); option order through an object state and a
//! reversed variant; an upstream 400 and 500 stored while the cell answers
//! 200; a held GPU row's 503 `gpu_hold` and a fallback's cell; an override
//! sent with only `model` changed, and Perturb refused beside one; an
//! override without `model` given the cell's; a repeated cell's 409, and two
//! posts of one cell at once reaching the model once; images as input
//! artifacts named in the stored request, every data URL reaching the
//! container as typed, an override's `lab-artifact:<id>` sent as the data URL
//! the kept image first came as; and the token gauge, which starts no
//! container unless asked and judges the route the hold swapped in.

mod price;

use lmgw_api_types::decision_lab::{
    DecisionForm, DecisionLabModels, DecisionMeasure, DecisionShape, ImageInput, OptionDraft,
    QuestionDraft, QuestionKind, StateMode, Variant, PERTURB_WITH_OVERRIDE,
};
use lmgw_api_types::lab_store::{LabCellResult, LabGroupView};
use lmgw_core::config::{AuxBackend, AuxKind, HoldFallbackMode, Protocol, UpstreamKind};
use lmgw_core::egress::openai_wire::SystemOneBody;
use lmgw_core::gguf::synth;
use lmgw_core::store::{self, NewAlias, NewAuxModel, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::common::Gw;
use crate::labs_store::typed_data_url;
use crate::stream_usage::row_after;
use crate::support::decision_world::{
    decision_world_on, healthy, DecisionWorld, DECISION, EMBED, LLAMA_ANSWER,
};

/// The CPU row with Laya's planned values: ctx = ubatch = batch = 8192.
pub const CPU_ROW: &str = "laya-cpu";

/// `/tokenize` answering one token per whitespace-separated word.
struct Words;

impl Respond for Words {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let v: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let n = v["content"]
            .as_str()
            .map_or(0, |s| s.split_whitespace().count());
        ResponseTemplate::new(200).set_body_json(json!({ "tokens": vec![1; n] }))
    }
}

pub struct Lab {
    pub w: DecisionWorld,
    pub container: MockServer,
    _dir: tempfile::TempDir,
}

impl Lab {
    pub fn gw(&self) -> &Gw {
        &self.w.gw
    }
}

/// Laya's header: non-causal, 8192 trained tokens, a 192-token head budget.
fn laya_header() -> synth::Header {
    let mut h = synth::Header::default();
    h.str("general.architecture", "modern-bert")
        .u32("modern-bert.context_length", 8192)
        .u32("modern-bert.block_count", 28)
        .bool_flag("modern-bert.attention.causal", false)
        .str("modern-bert.decision.type", "laya")
        .u32("modern-bert.decision.max_head_tokens", 192)
        .str("tokenizer.chat_template.systemone", "{{ state }}")
        .tensor("cls.output.weight");
    h
}

/// `decision_world` with a models dir holding Laya's header (both decision
/// rows read it), the CPU row beside the GPU one, `/tokenize`, and the
/// container answering every System One request with llama.cpp's answer.
pub async fn lab() -> Lab {
    lab_on(lmgw_core::state::AppState::init_for_tests().await.unwrap()).await
}

/// [`lab`] on `state`'s database.
pub async fn lab_on(state: lmgw_core::state::SharedState) -> Lab {
    let container = MockServer::start().await;
    healthy(&container).await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(Words)
        .mount(&container)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(LLAMA_ANSWER, "application/json"))
        .mount(&container)
        .await;
    let w = decision_world_on(state, &container.uri()).await;
    let dir = tempfile::tempdir().unwrap();
    laya_header().write_to(&dir.path().join("laya.gguf"));
    let mut s = w.state.snapshot().settings.clone();
    s.aux_router.models_dir = dir.path().display().to_string();
    store::save_settings(&w.state.db, &s).await.unwrap();
    store::insert_aux_model(
        &w.state.db,
        &NewAuxModel {
            model_id: CPU_ROW.into(),
            gguf_path: "laya.gguf".into(),
            kind: AuxKind::Decision,
            pooling: None,
            ctx_size: Some(8192),
            ubatch_size: Some(8192),
            batch_size: Some(8192),
            mmproj_path: None,
            backend: Some(AuxBackend::Cpu),
            threads: None,
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    Lab {
        w,
        container,
        _dir: dir,
    }
}

/// The ticket: a choice whose options are not alphabetical, a score and a
/// noul.
pub fn ticket() -> DecisionForm {
    DecisionForm {
        state_mode: StateMode::Text,
        state: "I was charged twice for order A-1001. Please refund one payment.".into(),
        images: vec![],
        questions: vec![
            QuestionDraft {
                id: "team".into(),
                instructions: "Which team should handle this ticket?".into(),
                kind: QuestionKind::Choice {
                    options: ["technical", "billing", "other"]
                        .iter()
                        .map(|n| OptionDraft {
                            name: (*n).into(),
                            description: if *n == "billing" {
                                "payments, refunds".into()
                            } else {
                                String::new()
                            },
                        })
                        .collect(),
                },
            },
            QuestionDraft {
                id: "urgency".into(),
                instructions: "How urgent?".into(),
                kind: QuestionKind::Score {
                    levels: vec!["later".into(), "today".into()],
                },
            },
            QuestionDraft {
                id: "angry".into(),
                instructions: "Is the customer upset?".into(),
                kind: QuestionKind::Noul {
                    yes: "upset".into(),
                    no: String::new(),
                },
            },
        ],
    }
}

pub async fn post(gw: &Gw, path: &str, body: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Ask `form` of `models` with `variants`; the stored group.
pub async fn ask(
    gw: &Gw,
    form: &DecisionForm,
    models: &[&str],
    variants: &[Variant],
) -> LabGroupView {
    let (status, body) = post(
        gw,
        "/decision-lab/api/groups",
        json!({"form": form, "models": models, "variants": variants}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

/// Run cell `n` of group `id`.
pub async fn cell(gw: &Gw, id: i64, n: usize) -> LabCellResult {
    let (status, body) = post(
        gw,
        &format!("/decision-lab/api/groups/{id}/cells/{n}"),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

/// The bodies the container received on `/v1/systemone`.
async fn received(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/systemone"))
        .map(|r| String::from_utf8(r.body.clone()).unwrap())
        .collect()
}

/// What the route forwards for `body`: `model` rewritten to `upstream`.
fn forwarded_as(body: &str, upstream: &str) -> String {
    String::from_utf8(
        SystemOneBody::parse(body.as_bytes())
            .unwrap()
            .with_model(upstream)
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn the_model_list_carries_a_non_causal_rows_facts_and_limits() {
    let l = lab().await;
    lmgw_core::ops::hold_set(&l.w.state, true).await.unwrap();
    let (status, body) = {
        let resp = l
            .gw()
            .client()
            .get(format!("{}/decision-lab/api/models", l.gw()))
            .send()
            .await
            .unwrap();
        (resp.status().as_u16(), resp.json::<Value>().await.unwrap())
    };
    assert_eq!(status, 200, "{body}");
    let list: DecisionLabModels = serde_json::from_value(body).unwrap();
    assert!(list.hold);
    let names: Vec<&str> = list.models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names.len(),
        2,
        "the two decision rows, not the embedder or reranker: {names:?}"
    );

    let cpu = list
        .models
        .iter()
        .find(|m| m.name == l.w.name(CPU_ROW))
        .unwrap();
    assert!(cpu.local && !cpu.held && !cpu.reads_images, "{cpu:?}");
    assert_eq!(cpu.state, None, "not running");
    let f = cpu.facts.as_ref().unwrap();
    assert_eq!(f.header_error, None);
    assert_eq!(
        (f.decision_type.as_deref(), f.shape, f.max_head_tokens),
        (Some("laya"), Some(DecisionShape::NonCausal), Some(192))
    );
    assert_eq!((f.ctx_size, f.ubatch, f.batch), (Some(8192), 8192, 8192));
    assert_eq!(serde_json::to_value(f.ubatch_source).unwrap(), "row");
    assert_eq!(serde_json::to_value(f.backend).unwrap(), "cpu");
    assert!(
        f.threads_in_effect.is_some_and(|n| n > 0),
        "the machine's cores: {f:?}"
    );
    assert_eq!(cpu.limits.len(), 1);
    assert_eq!(
        (cpu.limits[0].applies_to, cpu.limits[0].tokens),
        (DecisionMeasure::StateAndQuestion, Some(8192))
    );
    assert!(
        cpu.limits[0]
            .rule
            .starts_with("laya evaluates the whole prompt"),
        "{:?}",
        cpu.limits
    );

    // The GPU row sets no ubatch: llama.cpp's 512, and its batch lowered to
    // it in embedding mode. The hold pauses it, with no fallback.
    let gpu = list
        .models
        .iter()
        .find(|m| m.name == l.w.name(DECISION))
        .unwrap();
    let f = gpu.facts.as_ref().unwrap();
    assert_eq!((f.ubatch, f.batch), (512, 512));
    assert_eq!(
        serde_json::to_value(f.ubatch_source).unwrap(),
        "llama_cpp_default"
    );
    assert_eq!(serde_json::to_value(f.batch_source).unwrap(), "ubatch");
    assert_eq!(serde_json::to_value(f.backend).unwrap(), "gpu");
    assert!(gpu.held && gpu.hold_fallback.is_none(), "{gpu:?}");
    assert_eq!(gpu.limits[0].tokens, Some(512));
}

#[tokio::test]
async fn a_cell_dispatches_through_the_real_handler_and_keeps_its_bytes() {
    let l = lab().await;
    let laya = l.w.name(CPU_ROW);
    let form = ticket();
    let g = ask(l.gw(), &form, &[&laya], &[]).await;
    assert_eq!(g.plan.len(), 1);
    assert!(g.runs.is_empty(), "no runs yet");
    assert_eq!(
        g.title,
        "I was charged twice for order A-1001. Please refund one payment."
    );

    let r = cell(l.gw(), g.id, 0).await;
    assert_eq!(r.store_error, None);
    let run = r.run;
    let sent = form.body(&laya, Variant::AsWritten, &[]).unwrap();
    assert_eq!(run.request, sent, "the builder's bytes, kept as sent");
    assert_eq!(
        run.response.as_deref(),
        Some(LLAMA_ANSWER),
        "the answer verbatim"
    );
    assert_eq!(received(&l.container).await, [forwarded_as(&sent, CPU_ROW)]);
    assert_eq!(
        (
            run.http_status,
            run.answered_by.as_str(),
            run.upstream_model.as_deref()
        ),
        (200, laya.as_str(), Some("laya"))
    );
    assert_eq!((run.input_tokens, run.output_tokens), (Some(299), Some(0)));
    assert_eq!((run.error_code, run.fallback_reason), (None, None));
    assert!(run.id.is_some());

    let row = row_after(&l.w.state, 0).await;
    assert_eq!(
        (row.status, row.ingress_proto.as_str(), row.class.as_deref()),
        (200, "systemone", Some("aux")),
        "{row:?}"
    );

    let (status, body) = post(
        l.gw(),
        &format!("/decision-lab/api/groups/{}/cells/0", g.id),
        json!(null),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("cell_answered"))
    );
    let (status, _) = post(
        l.gw(),
        &format!("/decision-lab/api/groups/{}/cells/1", g.id),
        json!(null),
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = post(l.gw(), "/decision-lab/api/groups/999/cells/0", json!(null)).await;
    assert_eq!(status, 404);

    // The history has it, with its run.
    let resp = l
        .gw()
        .client()
        .get(format!("{}/labs/api/decision/groups/{}", l.gw(), g.id))
        .send()
        .await
        .unwrap();
    let kept: LabGroupView = resp.json().await.unwrap();
    assert_eq!(kept.runs.len(), 1);
    assert_eq!(kept.runs[0].request, sent);
}

#[tokio::test]
async fn option_order_survives_an_object_state_and_a_reversed_variant() {
    let l = lab().await;
    let laya = l.w.name(CPU_ROW);
    let mut form = ticket();
    form.state_mode = StateMode::Json;
    form.state = "{\"b\": 1,  \"a\": [2, 1]}".into();
    let g = ask(l.gw(), &form, &[&laya], &[Variant::Reversed]).await;
    assert_eq!(
        g.plan
            .iter()
            .map(|c| c.variant.as_str())
            .collect::<Vec<_>>(),
        ["as_written", "reversed"]
    );
    let first = cell(l.gw(), g.id, 0).await.run;
    let second = cell(l.gw(), g.id, 1).await.run;
    let got = received(&l.container).await;
    // The route's own rewrite compacts between the top-level members; the
    // state's value goes as typed.
    assert!(
        got[0].contains(r#""state":{"b": 1,  "a": [2, 1]},"#),
        "{}",
        got[0]
    );
    assert!(first
        .request
        .contains("  \"state\": {\"b\": 1,  \"a\": [2, 1]},\n"));
    let at = |s: &str, k: &str| s.find(&format!("\"{k}\":")).unwrap();
    assert!(
        at(&got[0], "technical") < at(&got[0], "billing")
            && at(&got[0], "billing") < at(&got[0], "other")
    );
    assert!(
        at(&got[1], "other") < at(&got[1], "billing")
            && at(&got[1], "billing") < at(&got[1], "technical")
    );
    assert_eq!(
        first.request,
        form.body(&laya, Variant::AsWritten, &[]).unwrap()
    );
    assert_eq!(
        second.request,
        form.body(&laya, Variant::Reversed, &[]).unwrap()
    );
    assert_eq!(second.variant, "reversed");
}

#[tokio::test]
async fn an_upstream_400_and_500_are_stored_while_the_cell_answers_200() {
    const UBATCH: &str = r#"{"error":{"code":500,"message":"input (8519 tokens) is too large to process. increase the physical batch size (current batch size: 8192)","type":"server_error"}}"#;
    const OPTIONS: &str = r#"{"error":{"code":400,"message":"too many options (256), this model supports at most 255","type":"invalid_request_error"}}"#;
    let l = lab().await;
    for (marker, status, body) in [("OVER-UBATCH", 500, UBATCH), ("TOO-MANY", 400, OPTIONS)] {
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .and(body_string_contains(marker))
            .respond_with(ResponseTemplate::new(status).set_body_raw(body, "application/json"))
            .with_priority(1)
            .mount(&l.container)
            .await;
    }
    let laya = l.w.name(CPU_ROW);
    // llama.cpp's numeric `code` repeats the status: its `type` is the code.
    for (marker, status, body, message, code) in [
        (
            "OVER-UBATCH",
            500,
            UBATCH,
            "is too large to process",
            "server_error",
        ),
        (
            "TOO-MANY",
            400,
            OPTIONS,
            "too many options (256)",
            "invalid_request_error",
        ),
    ] {
        let mut form = ticket();
        form.state = format!("{marker} {}", form.state);
        let g = ask(l.gw(), &form, &[&laya], &[]).await;
        let r = cell(l.gw(), g.id, 0).await;
        let run = r.run;
        assert_eq!(run.http_status, status);
        assert_eq!(run.response.as_deref(), Some(body), "verbatim");
        assert_eq!(run.error_code.as_deref(), Some(code));
        assert!(
            run.error_message.as_deref().unwrap().contains(message),
            "{run:?}"
        );
        assert!(run.id.is_some(), "a failure is part of the record");
        assert_eq!(run.upstream_model, None);
    }
}

/// A cloud upstream with the alias `jev`, answering System One as Jev, and
/// the GPU row `laya-jev` falling back to it under the hold. The upstream's
/// server, to keep it up.
async fn with_jev(l: &Lab) -> MockServer {
    let up = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": [
                {"id": "jev", "architecture": {"output_modalities": ["decisions"]}},
            ]})),
        )
        .mount(&up)
        .await;
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"model": "jev-1.13.0", "answers": {}, "usage": {"input_tokens": 9, "output_tokens": 3}}"#,
            "application/json",
        ))
        .mount(&up)
        .await;
    let upstream = store::insert_upstream(
        &l.w.state.db,
        &NewUpstream {
            name: "typesafe".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: up.uri(),
            api_key: Some("sk-up".into()),
            extra_headers: vec![],
            timeout_ms: 5_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &l.w.state.db,
        &NewAlias {
            alias: "jev".into(),
            upstream_id: upstream,
            upstream_model_id: "jev".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    store::insert_aux_model(
        &l.w.state.db,
        &NewAuxModel {
            model_id: "laya-jev".into(),
            gguf_path: "laya.gguf".into(),
            kind: AuxKind::Decision,
            pooling: None,
            ctx_size: None,
            ubatch_size: None,
            batch_size: None,
            mmproj_path: None,
            backend: None,
            threads: None,
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: HoldFallbackMode::Alias,
            hold_fallback: Some("jev".into()),
        },
    )
    .await
    .unwrap();
    l.w.state.reload_snapshot().await.unwrap();
    up
}

#[tokio::test]
async fn a_held_gpu_row_is_a_503_cell_and_a_fallbacks_cell_names_it() {
    let l = lab().await;
    let _up = with_jev(&l).await;
    lmgw_core::ops::hold_set(&l.w.state, true).await.unwrap();

    let held = l.w.name(DECISION);
    let fallen = l.w.name("laya-jev");
    let g = ask(l.gw(), &ticket(), &[&held, &fallen], &[]).await;
    let r = cell(l.gw(), g.id, 0).await.run;
    assert_eq!(
        (
            r.http_status,
            r.error_code.as_deref(),
            r.answered_by.as_str()
        ),
        (503, Some("gpu_hold"), held.as_str())
    );
    let r = cell(l.gw(), g.id, 1).await.run;
    assert_eq!(
        (
            r.http_status,
            r.answered_by.as_str(),
            r.fallback_reason.as_deref()
        ),
        (200, "jev", Some("hold"))
    );
    assert_eq!(r.model_asked, fallen);
    assert_eq!(r.upstream_model.as_deref(), Some("jev-1.13.0"));
    assert_eq!(
        r.headers.get("x-lmgw-fallback").map(String::as_str),
        Some("jev")
    );
    assert_eq!(l.w.podman.runs(), 0, "no container started under the hold");
}

#[tokio::test]
async fn an_override_goes_out_with_only_model_changed_and_perturb_is_refused_beside_it() {
    let l = lab().await;
    let laya = l.w.name(CPU_ROW);
    let kev = "decide/kev-4b";
    // Two options swapped by hand, spaced as typed, an unknown member kept.
    let over = format!(
        "{{\"model\": \"{laya}\",\n \"state\": \"refund please\", \"x_extra\": 1,\n \
         \"questions\": {{\"team\": {{\"type\": \"choice\", \"instructions\": \"Which?\",\n \
         \"criteria\": {{\"other\": null, \"billing\": \"refunds\"}}}}}}}}"
    );
    let (status, body) = post(
        l.gw(),
        "/decision-lab/api/groups",
        json!({"form": ticket(), "models": [laya, kev], "variants": ["reversed"], "override": over}),
    )
    .await;
    assert_eq!(
        (status, body["message"].as_str()),
        (400, Some(PERTURB_WITH_OVERRIDE))
    );

    let (status, body) = post(
        l.gw(),
        "/decision-lab/api/groups",
        json!({"form": ticket(), "models": [laya], "override": over}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let g: LabGroupView = serde_json::from_value(body).unwrap();
    assert_eq!(
        g.override_text.as_deref(),
        Some(over.as_str()),
        "stored as typed"
    );
    let run = cell(l.gw(), g.id, 0).await.run;
    let sent = forwarded_as(&over, &laya);
    assert_eq!(
        run.request, sent,
        "only model rewritten, by the handler's own rewrite"
    );
    assert_eq!(received(&l.container).await, [forwarded_as(&over, CPU_ROW)]);
    assert!(run
        .request
        .contains(r#""criteria": {"other": null, "billing": "refunds"}"#));

    // A malformed form is not checked with an override: the route says what
    // it says. Not a JSON object is the override's one check.
    let (status, body) = post(
        l.gw(),
        "/decision-lab/api/groups",
        json!({"form": {}, "models": [laya], "override": "[1, 2]"}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = post(
        l.gw(),
        "/decision-lab/api/groups",
        json!({"form": {"state": " "}, "models": [laya]}),
    )
    .await;
    assert_eq!(
        (
            status,
            body["message"].as_str().map(|m| m.starts_with("state:"))
        ),
        (400, Some(true))
    );
}

#[tokio::test]
async fn images_become_input_artifacts_named_in_the_stored_request() {
    let l = lab().await;
    let laya = l.w.name(CPU_ROW);
    let png = crate::labs_store::PNG;
    // Typed with a type no sniffer gives and a parameter: the row keeps the
    // sniffed type, the container gets the URL as typed.
    let url = typed_data_url("data:image/x-receipt;name=receipt.png;base64", png);
    let mut form = ticket();
    form.images = vec![ImageInput::DataUrl {
        name: "receipt.png".into(),
        url: url.clone(),
    }];
    let g = ask(l.gw(), &form, &[&laya], &[]).await;
    assert_eq!(g.artifacts.len(), 1);
    let a = &g.artifacts[0];
    assert_eq!(
        (a.name.as_str(), a.mime.as_str(), a.size),
        ("receipt.png", "image/png", png.len() as u64)
    );
    let stored: DecisionForm = serde_json::from_value(g.form.clone()).unwrap();
    assert_eq!(
        stored.images,
        [ImageInput::Artifact {
            id: a.id,
            name: "receipt.png".into()
        }],
        "a re-opened form sends the id"
    );

    let run = cell(l.gw(), g.id, 0).await.run;
    let reference = format!("lab-artifact:{}", a.id);
    assert_eq!(
        run.request,
        form.body(&laya, Variant::AsWritten, std::slice::from_ref(&reference))
            .unwrap()
    );
    assert!(!run.request.contains("base64"));
    let got = received(&l.container).await;
    assert_eq!(
        got,
        [forwarded_as(
            &form
                .body(&laya, Variant::AsWritten, std::slice::from_ref(&url))
                .unwrap(),
            CPU_ROW
        )]
    );
    assert!(
        got[0].contains(&format!("[\n    \"{url}\"\n  ]")),
        "{}",
        got[0]
    );

    // Re-asked from the stored form: nothing uploaded again, one file.
    let again = ask(l.gw(), &stored, &[&laya], &[]).await;
    assert_eq!(again.artifacts[0].sha256, a.sha256);
    assert_ne!(again.artifacts[0].id, a.id, "a row of its own");

    // An override's data URL is kept the same way.
    let over = format!(
        "{{\"model\": \"{laya}\", \"state\": \"s\", \"images\": [\"{url}\"], \"questions\": {{}}}}"
    );
    let (status, body) = post(
        l.gw(),
        "/decision-lab/api/groups",
        json!({"form": ticket(), "models": [laya], "override": over}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let g: LabGroupView = serde_json::from_value(body).unwrap();
    let id = g.artifacts[0].id;
    let stored = g.override_text.unwrap();
    assert_eq!(stored, over.replace(&url, &format!("lab-artifact:{id}")));
    let run = cell(l.gw(), g.id, 0).await.run;
    assert_eq!(run.request, forwarded_as(&stored, &laya));
    let got = received(&l.container).await;
    assert_eq!(got.last().unwrap(), &forwarded_as(&over, CPU_ROW));
}

#[tokio::test]
async fn an_override_naming_a_kept_image_sends_the_url_it_first_came_as() {
    let l = lab().await;
    let laya = l.w.name(CPU_ROW);
    let head = "data:image/x-receipt;name=receipt.png;base64";
    let url = typed_data_url(head, crate::labs_store::PNG);
    let mut form = ticket();
    form.images = vec![ImageInput::DataUrl {
        name: "receipt.png".into(),
        url: url.clone(),
    }];
    let first = ask(l.gw(), &form, &[&laya], &[]).await;
    let old = &first.artifacts[0];
    assert_eq!(old.data_url_head.as_deref(), Some(head));

    // "Edit as JSON" over the re-opened form: the image by its ref, here
    // named twice.
    let reference = format!("lab-artifact:{}", old.id);
    let over = format!(
        "{{\"model\": \"{laya}\", \"state\": \"s\",\n \"images\": [\"{reference}\", \"{reference}\"], \"questions\": {{}}}}"
    );
    // The re-opened form names it as well, as "Open in form" leaves it.
    let reopened: DecisionForm = serde_json::from_value(first.form.clone()).unwrap();
    let (status, body) = post(
        l.gw(),
        "/decision-lab/api/groups",
        json!({"form": reopened, "models": [laya], "override": over}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let g: LabGroupView = serde_json::from_value(body).unwrap();
    assert_eq!(
        g.artifacts.len(),
        1,
        "one row for the file the form and the override name"
    );
    let a = &g.artifacts[0];
    assert_ne!(a.id, old.id, "a row of its own");
    assert_eq!(
        (a.sha256.as_str(), a.data_url_head.as_deref()),
        (old.sha256.as_str(), Some(head)),
        "the same file, typed the same way"
    );
    let stored = g.override_text.clone().unwrap();
    assert_eq!(
        stored,
        over.replace(&reference, &format!("lab-artifact:{}", a.id)),
        "the new group's own row, every other byte as typed"
    );

    // The old group goes; the new one keeps its image.
    let (status, body) = post(
        l.gw(),
        &format!("/labs/api/decision/groups/{}/delete", first.id),
        json!(null),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let run = cell(l.gw(), g.id, 0).await.run;
    assert_eq!(run.http_status, 200, "{run:?}");
    assert_eq!(run.request, forwarded_as(&stored, &laya));
    assert_eq!(
        received(&l.container).await,
        [forwarded_as(&over.replace(&reference, &url), CPU_ROW)],
        "the URL as it was first typed, byte for byte"
    );

    // An id the store does not keep is the 400 that names it.
    let over = format!(
        "{{\"model\": \"{laya}\", \"state\": \"s\", \"images\": [\"lab-artifact:999999\"], \"questions\": {{}}}}"
    );
    let (status, body) = post(
        l.gw(),
        "/decision-lab/api/groups",
        json!({"form": ticket(), "models": [laya], "override": over}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("lab-artifact:999999")),
        "{body}"
    );
}

#[tokio::test]
async fn the_count_answers_per_piece_and_starts_no_container_unless_asked() {
    let l = lab().await;
    let laya = l.w.name(CPU_ROW);
    let mut form = ticket();
    form.images = vec![ImageInput::Artifact {
        id: 1,
        name: "x.png".into(),
    }];
    let count = |load: bool| {
        let form = form.clone();
        let laya = laya.clone();
        let gw = l.gw().clone();
        async move {
            let (status, body) = post(
                &gw,
                "/decision-lab/api/count",
                json!({"model": laya, "form": form, "load": load}),
            )
            .await;
            assert_eq!(status, 200, "{body}");
            body
        }
    };

    let body = count(false).await;
    assert_eq!(body["not_counted"], format!("{laya} is not loaded"));
    assert_eq!(l.w.podman.runs(), 0, "no container started");

    let body = count(true).await;
    assert_eq!(body["not_counted"], Value::Null, "{body}");
    assert_eq!(l.w.podman.runs(), 1, "asked to load: its container");
    // One token per word: the state's 11, the team question's instructions
    // and options (6 + 5), its options alone (5), the score's (2 + 2), the
    // noul's (4 + 1).
    assert_eq!(body["state"], 11, "{body}");
    assert_eq!(
        body["questions"],
        json!([
            {"id": "team", "tokens": 11, "option_span": 5},
            {"id": "urgency", "tokens": 4, "option_span": null},
            {"id": "angry", "tokens": 5, "option_span": null},
        ])
    );
    assert_eq!(body["approx"], json!(["flattened", "media_omitted"]));
    assert_eq!(
        body["estimates"][0]["tokens"], 22,
        "the state plus the largest question"
    );
    assert_eq!(body["estimates"][0]["over"], false);
    assert_eq!(body["estimates"][0]["limit"]["tokens"], 8192);

    // Loaded now: counted without asking.
    let body = count(false).await;
    assert_eq!(
        (body["not_counted"].clone(), body["state"].clone()),
        (Value::Null, json!(11))
    );
    assert_eq!(l.w.podman.runs(), 1);

    let (status, body) = post(
        l.gw(),
        "/decision-lab/api/count",
        json!({"model": l.w.name(EMBED), "form": ticket()}),
    )
    .await;
    assert_eq!((status, body["code"].as_str()), (404, Some("not_found")));
}

#[tokio::test]
async fn every_data_url_reaches_the_container_as_typed() {
    let l = lab().await;
    let laya = l.w.name(CPU_ROW);
    let png = typed_data_url(
        "data:image/x-receipt;name=receipt.png;base64",
        crate::labs_store::PNG,
    );
    // A text file: sniffed `text/plain; charset=utf-8`, typed without one.
    let text = typed_data_url("data:text/plain;base64", b"Order A-1001, paid twice.");
    // Escaped as typed: a plain JSON string would not give these bytes back,
    // so it stays inline.
    let escaped = typed_data_url("data:image\\/png;base64", crate::labs_store::PNG);
    let over = format!(
        "{{\"model\": \"{laya}\", \"state\": \"s\", \"images\": [\"{png}\", \"{escaped}\"],\n \
         \"files\": [\"{text}\"], \"questions\": {{}}}}"
    );
    let (status, body) = post(
        l.gw(),
        "/decision-lab/api/groups",
        json!({"form": ticket(), "models": [laya], "override": over}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let g: LabGroupView = serde_json::from_value(body).unwrap();
    let mimes: Vec<&str> = g.artifacts.iter().map(|a| a.mime.as_str()).collect();
    assert_eq!(
        mimes,
        ["image/png", "text/plain; charset=utf-8"],
        "the rows keep the sniffed types; the escaped one is no artifact"
    );
    let stored = g.override_text.clone().unwrap();
    assert!(stored.contains(&escaped) && !stored.contains(&png) && !stored.contains(&text));

    let run = cell(l.gw(), g.id, 0).await.run;
    assert_eq!(run.http_status, 200, "{run:?}");
    let got = received(&l.container).await;
    assert_eq!(
        got,
        [forwarded_as(&over, CPU_ROW)],
        "byte for byte as typed"
    );
    for url in [&png, &text, &escaped] {
        assert!(got[0].contains(&format!("\"{url}\"")), "{url}");
    }
}

#[tokio::test]
async fn an_override_without_model_is_sent_with_the_cells_model_first() {
    let l = lab().await;
    let laya = l.w.name(CPU_ROW);
    let over = "{\"state\": \"refund please\",\n \"questions\": {\"angry\": {\"type\": \"noul\", \
                \"instructions\": \"Upset?\"}}}";
    let (status, body) = post(
        l.gw(),
        "/decision-lab/api/groups",
        json!({"form": ticket(), "models": [laya], "override": over}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let g: LabGroupView = serde_json::from_value(body).unwrap();
    assert_eq!(g.override_text.as_deref(), Some(over), "stored as typed");

    let run = cell(l.gw(), g.id, 0).await.run;
    assert_eq!(
        (run.http_status, run.error_code.as_deref()),
        (200, None),
        "{run:?}"
    );
    let with_model = format!("{{\"model\":\"{laya}\",{}", &over[1..]);
    assert_eq!(run.request, forwarded_as(&with_model, &laya));
    assert!(run
        .request
        .starts_with(&format!("{{\"model\":\"{laya}\",\"state\":")));
    assert_eq!(
        received(&l.container).await,
        [forwarded_as(&with_model, CPU_ROW)]
    );
}

#[tokio::test]
async fn two_posts_of_one_cell_reach_the_model_once() {
    let l = lab().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(body_string_contains("SLOW"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(LLAMA_ANSWER, "application/json")
                .set_delay(std::time::Duration::from_millis(600)),
        )
        .with_priority(1)
        .mount(&l.container)
        .await;
    let laya = l.w.name(CPU_ROW);
    let mut form = ticket();
    form.state = format!("SLOW {}", form.state);
    let g = ask(l.gw(), &form, &[&laya], &[]).await;
    let at = format!("/decision-lab/api/groups/{}/cells/0", g.id);
    let (a, b) = tokio::join!(
        post(l.gw(), &at, json!(null)),
        post(l.gw(), &at, json!(null))
    );
    let mut statuses = [a.0, b.0];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409], "{a:?} {b:?}");
    let conflict = if a.0 == 409 { &a.1 } else { &b.1 };
    assert_eq!(conflict["code"], "cell_answered");
    assert_eq!(received(&l.container).await.len(), 1, "one dispatch");

    // Answered now: the next post is the 409 too, still without a dispatch.
    let (status, body) = post(l.gw(), &at, json!(null)).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("cell_answered"))
    );
    assert_eq!(received(&l.container).await.len(), 1);
}

#[tokio::test]
async fn the_count_judges_the_route_the_hold_swapped_in() {
    let l = lab().await;
    let _up = with_jev(&l).await;
    let fallen = l.w.name("laya-jev");
    let count = |model: String| {
        let gw = l.gw().clone();
        async move {
            let (status, body) = post(
                &gw,
                "/decision-lab/api/count",
                json!({"model": model, "form": ticket()}),
            )
            .await;
            assert_eq!(status, 200, "{body}");
            body
        }
    };

    // Not held, not loaded: not counted, nothing started.
    let body = count(fallen.clone()).await;
    assert_eq!(body["not_counted"], format!("{fallen} is not loaded"));
    assert_eq!(l.w.podman.runs(), 0);

    // Held: the count goes to the cloud fallback, which loads nothing.
    lmgw_core::ops::hold_set(&l.w.state, true).await.unwrap();
    let body = count(fallen.clone()).await;
    assert_eq!(body["not_counted"], Value::Null, "{body}");
    assert_eq!(body["answered_by"], "jev");
    assert!(body["state"].as_u64().is_some_and(|n| n > 0), "{body}");
    // Held with no fallback: the hold's refusal, nothing started.
    let body = count(l.w.name(DECISION)).await;
    let why = body["not_counted"].as_str().unwrap_or_default();
    assert!(why.starts_with("the count failed"), "{body}");
    assert_eq!(l.w.podman.runs(), 0, "no container started");
}
