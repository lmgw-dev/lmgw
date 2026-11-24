//! Model help with a **local** page-reading model, on `support/gpu_world.rs`
//! (document-conversion design §7.5, §7.7, D14, D20, D27, WP6): the GPU
//! hold without a fallback (`model_held`, tier 0 kept, nothing started),
//! the hold with a fallback (the fallback answers, with the page's image
//! and lmgw's Markdown prompt, and the answer says which model did), a
//! fallback that takes no pictures (nothing is sent to it), the local
//! model's own slots as the bound, its `max_tokens` from its real context
//! less the counted prompt, and a hold switched on or released in the
//! middle of a request.

use std::time::Duration;

use base64::Engine as _;
use lmgw_core::extract::convert::model_help::prompts;
use serde_json::{json, Value};

use crate::chat_attach_kinds::pdf_bytes;
use crate::common::{self, Gw};
use crate::support::gpu_world::{Gpu, GIB};

/// The local chat row chosen as the page-reading model.
const LOCAL: &str = "chat-model";

fn has_poppler() -> bool {
    ["pdftotext", "pdftoppm"]
        .iter()
        .all(|t| std::process::Command::new(t).arg("-v").output().is_ok())
}

/// The local row as the page-reading model, the cloud alias `cloud-chat`
/// beside it, and `fallback` as the local row's fallback.
async fn world(fallback: Option<&str>) -> Gpu {
    let g = Gpu::new(8 * GIB, 2, 5).await;
    g.model(LOCAL, GIB).await;
    g.cloud("cloud-chat", None).await;
    crate::support::fallbacks::every_chat_model_falls_back_to(&g.state, fallback).await;
    g.state
        .doc_tools
        .set_search_path(std::env::var_os("PATH").unwrap_or_default());
    let mut s = g.state.snapshot().settings.clone();
    s.documents.page_reader = LOCAL.into();
    lmgw_core::store::save_settings(&g.state.db, &s)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    // The fake container renders a prompt with a picture, and says what a
    // llama-server says about itself.
    {
        let mut w = g.world();
        w.projectors.insert(LOCAL.into());
        w.prompt_tokens = 1142;
        w.props.insert(
            LOCAL.into(),
            json!({
                "default_generation_settings": {"n_ctx": 8192},
                "modalities": {"vision": true, "audio": false},
            }),
        );
    }
    g
}

fn request(pages: &[Option<&str>]) -> Value {
    json!({
        "files": [{
            "name": "example-scan.pdf",
            "data_base64": base64::engine::general_purpose::STANDARD.encode(pdf_bytes(pages)),
        }],
        "output": "json",
    })
}

async fn post(gw: &Gw, body: &Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

async fn convert(gw: &Gw, body: &Value) -> Value {
    let (status, a) = post(gw, body).await;
    assert_eq!(status, 200, "{a}");
    a
}

/// The text part of a chat body's one message, and whether it carries a
/// page image.
fn sent(body: &Value) -> (String, bool) {
    let parts = body["messages"][0]["content"].as_array().unwrap();
    let text = parts.iter().find_map(|p| p["text"].as_str());
    let image = parts.iter().any(|p| {
        p["image_url"]["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("data:image/png;base64,"))
    });
    (text.unwrap_or_default().to_string(), image)
}

/// The owner's capability facts of a cloud chat model that takes pictures,
/// or does not.
fn caps(vision: bool) -> Value {
    let modalities = if vision {
        vec!["text", "image"]
    } else {
        vec!["text"]
    };
    json!({"capabilities": {
        "task": "chat",
        "endpoints": ["/v1/chat/completions"],
        "input_modalities": modalities,
        "vision": vision,
        "source": "owner",
    }})
}

fn warning<'a>(doc: &'a Value, code: &str) -> Option<&'a Value> {
    doc["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == code)
}

#[tokio::test]
async fn under_the_hold_without_a_fallback_pages_keep_tier_0_and_nothing_starts() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let g = world(None).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let gw = common::serve(g.state.clone()).await;
    let a = convert(&gw, &request(&[Some("Page one"), None, None])).await;
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    let held = warning(d, "model_held").expect("the held pages are named");
    assert_eq!(held["parts"], json!([2, 3]));
    assert!(
        held["message"]
            .as_str()
            .unwrap()
            .contains("holding the GPU"),
        "{held}"
    );
    assert_eq!(d["structure"][0]["blocks"][0]["text"], "Page one");
    assert_eq!(d["stats"]["model_calls"], 0);
    assert!(g.world().runs.is_empty(), "no container was started");
    assert!(g.cloud_bodies().await.is_empty());

    // S6: a scan alone fails as the held model it is: 503 `model_held`,
    // not a 415 "nothing reads scanned pages".
    let (status, a) = post(&gw, &request(&[None])).await;
    let d = &a["documents"][0];
    assert_eq!(status, 503, "{a}");
    assert_eq!(
        (&d["status"], &d["error"]["code"]),
        (&json!("failed"), &json!("model_held"))
    );
    let message = d["error"]["message"].as_str().unwrap();
    assert!(message.contains("holding the GPU"), "{message}");
    assert!(!message.contains("not read as text yet"), "{message}");
    assert!(g.world().runs.is_empty(), "no container was started");

    // Released: the next conversion's pages go to the model again.
    lmgw_core::ops::hold_set(&g.state, false).await.unwrap();
    let a = convert(&gw, &request(&[Some("Page one"), None])).await;
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert_eq!(
        d["structure"][1]["blocks"][0]["text"],
        format!("ok from {LOCAL}")
    );
}

#[tokio::test]
async fn under_the_hold_the_fallback_answers_and_the_answer_says_which_model() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let g = world(Some("cloud-chat")).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let gw = common::serve(g.state.clone()).await;
    let a = convert(&gw, &request(&[Some("Page one"), None])).await;
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert_eq!(d["warnings"], json!([]));
    let block = &d["structure"][1]["blocks"][0];
    assert_eq!(
        (&block["text"], &block["source"]),
        (&json!("from the cloud"), &json!("vision"))
    );
    let by = &d["stats"]["models"][0];
    assert_eq!(by["alias"], "cloud-chat");
    assert_eq!(by["answered_for"], LOCAL);
    assert_eq!(by["bound_from"], "tool_pool");
    assert!(g.world().runs.is_empty(), "the held model was not started");
    // B1: the fallback was sent the page itself, with lmgw's prompt: an
    // image, never a placeholder for one.
    let bodies = g.cloud_bodies().await;
    assert_eq!(bodies.len(), 1);
    let (text, image) = sent(&bodies[0]);
    assert!(
        image,
        "the fallback's request carries the page image: {}",
        bodies[0]
    );
    assert_eq!(text, prompts::IMAGE_PROMPT);
    assert!(!bodies[0].to_string().contains("omitted"), "{}", bodies[0]);

    // What Settings › Documents says beside the choice, before it happens:
    // the model is local, its fallback is outside, at a named service.
    let caps = lmgw_core::extract::convert::doc_tools(&g.state)
        .await
        .unwrap();
    assert!(caps.page_reader.local && caps.page_reader.outside.is_none());
    let fallback = caps.page_reader.fallback.expect("the fallback is named");
    assert_eq!(
        (fallback.alias.as_str(), fallback.local),
        ("cloud-chat", false)
    );
    let outside = fallback.outside.expect("where its pages are sent");
    assert_eq!(
        (outside.service.as_str(), outside.host.as_str()),
        ("cloud-cloud-chat", "127.0.0.1")
    );
}

/// B1: a fallback that takes no pictures is never sent a page. The gate
/// would give it a text placeholder for the image, and whatever it answered
/// would become the page's text.
#[tokio::test]
async fn a_fallback_that_cannot_see_is_sent_nothing_and_pages_keep_tier_0() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let g = world(None).await;
    g.cloud("blind-chat", Some(caps(false))).await;
    crate::support::fallbacks::every_chat_model_falls_back_to(&g.state, Some("blind-chat")).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let gw = common::serve(g.state.clone()).await;
    let a = convert(&gw, &request(&[Some("Page one"), None, None])).await;
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    assert!(warning(d, "model_held").is_none(), "{d}");
    let blind = warning(d, "model_cannot_see").expect("the pages are named");
    assert_eq!(blind["parts"], json!([2, 3]));
    let message = blind["message"].as_str().unwrap();
    assert!(
        message.contains("its fallback 'blind-chat' answers in its place and cannot read pages"),
        "{message}"
    );
    // Tier 0 is what the answer holds: the page with text, and no word of
    // a model that never saw the other two.
    assert_eq!(d["structure"][0]["blocks"][0]["text"], "Page one");
    assert_eq!(d["structure"][1]["blocks"], json!([]));
    assert_eq!(d["stats"]["parts_by_source"]["vision"], 0);
    assert_eq!(d["stats"]["model_calls"], 0);
    assert!(g.cloud_bodies().await.is_empty(), "nothing was sent to it");
    assert!(g.world().runs.is_empty(), "the held model was not started");

    // A scan alone: the model cannot run now, 503, naming the fallback.
    let (status, a) = post(&gw, &request(&[None])).await;
    let d = &a["documents"][0];
    assert_eq!(
        (status, &d["error"]["code"]),
        (503, &json!("model_held")),
        "{a}"
    );
    assert!(d["error"]["message"]
        .as_str()
        .unwrap()
        .contains("'blind-chat'"));
    assert!(g.cloud_bodies().await.is_empty());

    // Settings › Documents says it before it happens.
    let caps = lmgw_core::extract::convert::doc_tools(&g.state)
        .await
        .unwrap();
    let fallback = caps.page_reader.fallback.expect("the fallback is named");
    assert_eq!(
        (fallback.alias.as_str(), fallback.sees),
        ("blind-chat", Some(false))
    );
}

/// B1's second face: the prompt and the reader follow the model that
/// answers. A fallback is asked with lmgw's Markdown prompt and read as
/// Markdown, whatever the chosen model answers in.
#[tokio::test]
async fn a_fallback_is_asked_in_markdown_though_the_chosen_model_answers_in_doctags() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let g = world(Some("cloud-chat")).await;
    let mut s = g.state.snapshot().settings.clone();
    s.documents.page_reader_doctags = true;
    lmgw_core::store::save_settings(&g.state.db, &s)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let gw = common::serve(g.state.clone()).await;
    let a = convert(&gw, &request(&[None])).await;
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    // No "started without --special" note for a model that is no DocTags
    // model.
    assert_eq!(d["warnings"], json!([]));
    assert_eq!(d["structure"][0]["blocks"][0]["text"], "from the cloud");
    let (text, image) = sent(&g.cloud_bodies().await[0]);
    assert_eq!(text, prompts::IMAGE_PROMPT);
    assert!(image);

    // Released: the chosen model itself is asked docling's way.
    lmgw_core::ops::hold_set(&g.state, false).await.unwrap();
    convert(&gw, &request(&[None])).await;
    let local: Vec<Value> = g
        .posted()
        .await
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(local.len(), 1);
    assert_eq!(sent(&local[0]).0, prompts::DOCTAGS_PROMPT);
}

#[tokio::test]
async fn a_local_model_s_slots_bound_the_calls_and_its_context_the_answer() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let g = world(None).await;
    let gw = common::serve(g.state.clone()).await;
    let a = convert(&gw, &request(&[None, Some("Page two"), None])).await;
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert_eq!(
        d["stats"]["parts_by_source"],
        json!({"text_layer": 1, "ocr": 0, "vision": 2, "native": 0, "asr": 0})
    );
    let by = &d["stats"]["models"][0];
    assert_eq!(by["alias"], LOCAL);
    assert!(by.get("answered_for").is_none(), "{by}");
    // The row leaves its slots to llama-server: its own 4.
    assert_eq!(
        (&by["in_flight_bound"], &by["bound_from"]),
        (&json!(4), &json!("row_slots"))
    );
    assert_eq!(g.world().runs.len(), 1, "one start for the request's pages");
    // The prompt was counted on the running server, and the answer bound is
    // the server's context less it: 8192 - 1142.
    assert!(!g.world().templated.is_empty() && !g.world().tokenized.is_empty());
    let chats: Vec<Value> = g
        .posted()
        .await
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(chats.len(), 2);
    for c in &chats {
        assert_eq!(c["max_tokens"], 7050, "{c}");
    }
}

/// §7.7: a hold switched on while pages are being read. The calls already
/// with the model end as the hold ends them; every page not yet sent takes
/// the held path.
#[tokio::test]
async fn a_hold_switched_on_mid_request_holds_the_pages_not_yet_sent() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let g = world(None).await;
    g.world().chat_delay = Duration::from_millis(1500);
    let gw = common::serve(g.state.clone()).await;
    // One page with text, so the file is read whatever becomes of the nine.
    let mut pages = vec![Some("Page one")];
    pages.extend([None; 9]);
    let body = request(&pages);
    let client = gw.clone();
    let running = tokio::spawn(async move { convert(&client, &body).await });
    // The model's four slots are taken: the other five pages wait.
    for _ in 0..1000 {
        if g.world().chats.len() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(g.world().chats.len(), 4, "four calls in flight, no more");
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let a = running.await.unwrap();
    let d = &a["documents"][0];
    let held = warning(d, "model_held").expect("the pages not yet sent are held");
    let held_pages = held["parts"].as_array().unwrap().len();
    assert!(held_pages >= 5, "{held}");
    assert_eq!(g.world().chats.len(), 4, "no page was sent under the hold");
    assert_eq!(d["stats"]["model_calls"], 4, "{d}");
}

/// §7.7: a hold released while pages wait. The call already with the
/// fallback ends there; every page not yet sent goes to the model again.
#[tokio::test]
async fn a_hold_released_mid_request_sends_the_pages_not_yet_sent_to_the_model_again() {
    use lmgw_core::config::{Protocol, UpstreamKind};
    use lmgw_core::store::{self, NewAlias, NewUpstream};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let g = world(None).await;
    // A fallback that takes its time, so pages wait in line behind it.
    let slow = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "slow",
                    "choices": [{"index": 0, "finish_reason": "stop",
                                 "message": {"role": "assistant", "content": "from the slow cloud"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                }))
                .set_delay(Duration::from_millis(1500)),
        )
        .mount(&slow)
        .await;
    let up = store::insert_upstream(
        &g.state.db,
        &NewUpstream {
            name: "example-slow".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", slow.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
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
        &g.state.db,
        &NewAlias {
            alias: "slow-cloud".into(),
            upstream_id: up,
            upstream_model_id: "slow".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    crate::support::fallbacks::every_chat_model_falls_back_to(&g.state, Some("slow-cloud")).await;
    // One call in flight at the fallback: the tool pool's size is its bound.
    let mut s = g.state.snapshot().settings.clone();
    s.documents.tool_slots = 1;
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let gw = common::serve(g.state.clone()).await;
    let body = request(&[None, None, None, None]);
    let client = gw.clone();
    let running = tokio::spawn(async move { convert(&client, &body).await });
    // The first page is with the fallback; three wait for their turn.
    let chats = || async {
        let got = slow.received_requests().await.unwrap_or_default();
        got.iter()
            .filter(|r| r.url.path() == "/v1/chat/completions")
            .count()
    };
    for _ in 0..1000 {
        if chats().await >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(chats().await, 1);
    lmgw_core::ops::hold_set(&g.state, false).await.unwrap();
    let a = running.await.unwrap();
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert_eq!(
        chats().await,
        1,
        "no page was sent to the fallback after the release"
    );
    let models = d["stats"]["models"].as_array().unwrap();
    let by = |alias: &str| models.iter().find(|m| m["alias"] == alias).unwrap();
    assert_eq!(
        (
            &by("slow-cloud")["calls"],
            &by("slow-cloud")["answered_for"]
        ),
        (&json!(1), &json!(LOCAL))
    );
    assert_eq!(
        (&by(LOCAL)["calls"], &by(LOCAL)["parts_read"]),
        (&json!(3), &json!(3))
    );
    assert_eq!(by(LOCAL)["bound_from"], "row_slots");
    let texts: Vec<&str> = d["structure"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["blocks"][0]["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts
            .iter()
            .filter(|t| **t == "from the slow cloud")
            .count(),
        1
    );
    assert_eq!(
        texts
            .iter()
            .filter(|t| **t == format!("ok from {LOCAL}"))
            .count(),
        3
    );
}

/// Review nit 2: the key's policy is asked before a route is opened, so a
/// key that is over its budget never cold-loads the model.
#[tokio::test]
async fn an_over_budget_key_never_starts_the_model() {
    use lmgw_core::config::hash_api_key;

    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    const KEY: &str = "lmgw-example-key";
    let g = world(None).await;
    let mut s = g.state.snapshot().settings.clone();
    s.auth_enabled = true;
    lmgw_core::store::save_settings(&g.state.db, &s)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns,
             budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit)
         VALUES ('example-agent', ?1, 1, 'all', '', 1000000, 'month', 0, 0, 0)",
    )
    .bind(hash_api_key(KEY))
    .execute(&g.state.db)
    .await
    .unwrap();
    // The budget is spent: one priced row against the key this hour.
    sqlx::query(
        "INSERT INTO usage_hourly (bucket_utc, key_id, alias, upstream_id, class, outcome,
             requests, cost_micro)
         VALUES (strftime('%Y-%m-%dT%H','now'),
                 (SELECT id FROM api_keys WHERE name='example-agent'), 'a', 1, 'chat', 'ok',
                 1, 2000000)",
    )
    .execute(&g.state.db)
    .await
    .unwrap();
    g.state.reload_snapshot().await.unwrap();
    let gw = common::serve(g.state.clone()).await;
    let r = reqwest::Client::new()
        .post(format!("{gw}/v1/documents/convert"))
        .bearer_auth(KEY)
        .json(&request(&[Some("Page one"), None]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let a: Value = r.json().await.unwrap();
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    let refused = warning(d, "model_not_allowed").expect("the page is named");
    assert_eq!(refused["parts"], json!([2]));
    assert!(
        refused["message"].as_str().unwrap().contains("budget"),
        "{refused}"
    );
    assert!(
        g.world().runs.is_empty(),
        "the model was started for a key over its budget"
    );
}
