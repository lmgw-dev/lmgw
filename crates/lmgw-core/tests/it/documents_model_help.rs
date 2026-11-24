//! Model help on `POST /v1/documents/convert` (document-conversion design
//! §3 tier 1, §7.5, WP6) against a mock OpenAI-compatible upstream: which
//! pages go to the chosen page-reading model, what is sent (the page image
//! and a prompt, nothing about the caller), the answer as blocks (markdown
//! and DocTags), a runaway as a failed page, the usage row, and a request
//! without a chosen model. The key's policy, the bound on calls in flight,
//! cancellation, jobs and the docling route are `documents_model_calls.rs`;
//! a local model and the GPU hold `documents_model_hold.rs`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use lmgw_core::config::{Protocol, UpstreamKind};
use lmgw_core::extract::convert::model_help::prompts;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::chat_attach_kinds::pdf_bytes;
use crate::common::{serve, Gw};

pub(crate) const PATH: &str = "/v1/documents/convert";
pub(crate) const ALIAS: &str = "example-vision";
const MARKDOWN: &str = "# Scanned page\n\nRead by the model.\n\n| Item | Cost |\n| --- | --- |\n\
                        | Pens | 3 |\n\n$$\nE = mc^2\n$$";

pub(crate) fn has_poppler() -> bool {
    ["pdftotext", "pdftoppm"]
        .iter()
        .all(|t| std::process::Command::new(t).arg("-v").output().is_ok())
}

/// One canned answer of the mock model.
enum Canned {
    /// Text with its `finish_reason`.
    Text(String, &'static str),
    /// OpenAI's `message.refusal`, which arrives with `finish_reason: stop`.
    Refusal(String),
    /// An HTTP error.
    Status(u16),
}

/// The mock page-reading model: its canned answers in order (then
/// [`MARKDOWN`]), and when each call arrived.
pub(crate) struct Model {
    pub(crate) mock: MockServer,
    answers: Arc<Mutex<VecDeque<Canned>>>,
    pub(crate) arrivals: Arc<Mutex<Vec<Instant>>>,
}

impl Model {
    pub(crate) async fn start(delay: Duration) -> Self {
        let mock = MockServer::start().await;
        let answers: Arc<Mutex<VecDeque<Canned>>> = Arc::default();
        let arrivals: Arc<Mutex<Vec<Instant>>> = Arc::default();
        let (a, t) = (answers.clone(), arrivals.clone());
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_: &Request| {
                t.lock().unwrap().push(Instant::now());
                let canned = a
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Canned::Text(MARKDOWN.to_string(), "stop"));
                let (message, finish) = match canned {
                    Canned::Text(text, finish) => {
                        (json!({"role": "assistant", "content": text}), finish)
                    }
                    Canned::Refusal(text) => (
                        json!({"role": "assistant", "content": null, "refusal": text}),
                        "stop",
                    ),
                    Canned::Status(status) => {
                        return ResponseTemplate::new(status)
                            .set_body_json(json!({"error": {"message": "example upstream error",
                                                            "type": "server_error"}}))
                            .set_delay(delay)
                    }
                };
                ResponseTemplate::new(200)
                    .set_body_json(json!({
                        "id": "c", "object": "chat.completion", "created": 0,
                        "model": "vision-tgt",
                        "choices": [{"index": 0, "finish_reason": finish, "message": message}],
                        "usage": {"prompt_tokens": 1200, "completion_tokens": 300,
                                  "total_tokens": 1500},
                    }))
                    .set_delay(delay)
            })
            .mount(&mock)
            .await;
        Self {
            mock,
            answers,
            arrivals,
        }
    }

    fn will(&self, canned: Canned) {
        self.answers.lock().unwrap().push_back(canned);
    }

    pub(crate) fn will_answer(&self, text: &str, finish: &'static str) {
        self.will(Canned::Text(text.to_string(), finish));
    }

    /// The next call is refused by the model, in the model's own words.
    pub(crate) fn will_refuse(&self, text: &str) {
        self.will(Canned::Refusal(text.to_string()));
    }

    /// The next call is answered with the HTTP error `status`.
    pub(crate) fn will_fail(&self, status: u16) {
        self.will(Canned::Status(status));
    }

    /// The chat bodies it got, in arrival order.
    pub(crate) async fn bodies(&self) -> Vec<Value> {
        self.mock
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/v1/chat/completions")
            .map(|r| serde_json::from_slice(&r.body).unwrap_or_default())
            .collect()
    }
}

/// The text part and the image part of a page call's one message.
pub(crate) fn sent(body: &Value) -> (String, String) {
    let parts = body["messages"][0]["content"].as_array().unwrap();
    let text = parts
        .iter()
        .find_map(|p| p["text"].as_str())
        .unwrap_or_default()
        .to_string();
    let image = parts
        .iter()
        .find_map(|p| p["image_url"]["url"].as_str())
        .unwrap_or_default()
        .to_string();
    (text, image)
}

pub(crate) async fn settings(
    state: &SharedState,
    set: impl FnOnce(&mut lmgw_core::config::Settings),
) {
    let mut s = state.snapshot().settings.clone();
    set(&mut s);
    store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
}

/// A state whose alias [`ALIAS`] is the mock model; chosen as the
/// page-reading model when `chosen`.
pub(crate) async fn state_with(m: &Model, chosen: bool) -> SharedState {
    state_on(&format!("{}/v1", m.mock.uri()), chosen).await
}

/// [`state_with`] for an upstream at `base_url`.
pub(crate) async fn state_on(base_url: &str, chosen: bool) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    state
        .doc_tools
        .set_search_path(std::env::var_os("PATH").unwrap_or_default());
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "example-up".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: base_url.to_string(),
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
        &state.db,
        &NewAlias {
            alias: ALIAS.into(),
            upstream_id: up,
            upstream_model_id: "vision-tgt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    if chosen {
        settings(&state, |s| s.documents.page_reader = ALIAS.into()).await;
    }
    state
}

pub(crate) async fn gateway(m: &Model) -> (SharedState, Gw) {
    let state = state_with(m, true).await;
    let gw = serve(state.clone()).await;
    (state, gw)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub(crate) fn request(pages: &[Option<&str>], options: Value) -> Value {
    let mut body = json!({
        "files": [{"name": "example-scan.pdf", "data_base64": b64(&pdf_bytes(pages))}],
    });
    for (k, v) in options.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}

pub(crate) async fn convert(gw: &Gw, body: &Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}{PATH}"))
        .json(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

pub(crate) fn codes(doc: &Value) -> Vec<(String, Vec<u64>)> {
    doc["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["code"].as_str().unwrap().to_string(),
                w["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_u64)
                    .collect(),
            )
        })
        .collect()
}

async fn page_rows(state: &SharedState) -> Vec<store::RequestLogRow> {
    page_rows_of(state, ALIAS, 1).await
}

/// The `documents-page` rows written for `alias`, once there are at least
/// `at_least` of them (a dropped call's row is written from a task of its
/// own).
pub(crate) async fn page_rows_of(
    state: &SharedState,
    alias: &str,
    at_least: usize,
) -> Vec<store::RequestLogRow> {
    let mut rows = Vec::new();
    for _ in 0..300 {
        rows = store::query_logs(
            &state.db,
            &store::LogFilter {
                alias: Some(alias.into()),
                limit: 50,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        rows.retain(|r| r.ingress_proto == "documents-page");
        if rows.len() >= at_least {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    rows
}

#[tokio::test]
async fn a_page_without_text_is_read_by_the_chosen_model_as_blocks() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let (state, gw) = gateway(&m).await;
    let body = request(&[Some("Page one"), None], json!({"output": "json"}));
    let (status, a) = convert(&gw, &body).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert_eq!(d["warnings"], json!([]));
    // Page 1 keeps its text layer; page 2 is the model's blocks.
    assert_eq!(d["structure"][0]["blocks"][0]["source"], "text_layer");
    let blocks = d["structure"][1]["blocks"].as_array().unwrap();
    let kinds: Vec<&str> = blocks.iter().map(|b| b["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["heading", "paragraph", "table", "formula"]);
    assert_eq!(blocks[0]["text"], "Scanned page");
    assert_eq!(blocks[3]["text"], "E = mc^2");
    assert!(blocks.iter().all(|b| b["source"] == "vision"), "{blocks:?}");
    let stats = &d["stats"];
    assert_eq!(
        stats["parts_by_source"],
        json!({"text_layer": 1, "ocr": 0, "vision": 1, "native": 0, "asr": 0})
    );
    assert_eq!(stats["model_calls"], 1);
    let by = &stats["models"][0];
    assert_eq!(
        (&by["alias"], &by["calls"], &by["parts_read"]),
        (&json!(ALIAS), &json!(1), &json!(1))
    );
    assert_eq!(by["bound_from"], "tool_pool");
    assert!(by.get("answered_for").is_none(), "{by}");

    // What was sent: the page's image and the prompt, nothing of the caller.
    let bodies = m.bodies().await;
    assert_eq!(bodies.len(), 1, "one call for the one page without text");
    let (text, image) = sent(&bodies[0]);
    assert_eq!(text, prompts::IMAGE_PROMPT);
    assert!(image.starts_with("data:image/png;base64,"), "{image:.40}");
    let raw = bodies[0].to_string();
    assert!(
        !raw.contains("example-scan") && !raw.contains("owner:"),
        "{raw:.200}"
    );
    assert_eq!(bodies[0]["temperature"], 0.0);
    // The mock model's context is not known to lmgw: no bound is invented.
    assert!(bodies[0].get("max_tokens").is_none(), "{}", bodies[0]);

    // One usage row for the call, the caller's, with counts and no content.
    let rows = page_rows(&state).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].ingress_proto, "documents-page");
    assert_eq!((rows[0].status, rows[0].prompt_tokens), (200, Some(1200)));
    assert!(rows[0].key_id.is_some(), "the call is the caller's key's");
    assert!(rows[0].error_msg.is_none());
}

#[tokio::test]
async fn model_help_picks_the_pages_and_off_asks_nothing() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let (_state, gw) = gateway(&m).await;
    let pages = [Some("Alpha"), None, Some("Gamma")];

    // Off: no call; the unread page's warning says a model would read it
    // (text recognition off, so that nothing reads it).
    let off = json!({"model_help": "off", "ocr": "off"});
    let (_, a) = convert(&gw, &request(&pages, off)).await;
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial");
    assert!(m.bodies().await.is_empty());
    let message = d["warnings"][0]["message"].as_str().unwrap();
    assert!(message.contains("page-reading model"), "{message}");
    assert_eq!(d["stats"]["model_calls"], 0);

    // The setting's choice: only the page nothing read.
    let (_, a) = convert(&gw, &request(&pages, json!({}))).await;
    assert_eq!(a["documents"][0]["status"], "converted");
    assert_eq!(m.bodies().await.len(), 1);
    assert_eq!(a["documents"][0]["stats"]["parts_by_source"]["vision"], 1);

    // Every page: the pages with text go with their text to copy from.
    let (_, a) = convert(&gw, &request(&pages, json!({"model_help": "every_page"}))).await;
    let d = &a["documents"][0];
    assert_eq!(d["stats"]["parts_by_source"]["vision"], 3, "{d}");
    assert_eq!(d["stats"]["model_calls"], 3);
    let bodies = m.bodies().await;
    assert_eq!(bodies.len(), 4);
    let mut texts: Vec<String> = bodies[1..].iter().map(|b| sent(b).0).collect();
    texts.sort();
    assert!(texts[0].ends_with("Extracted text:\nAlpha"), "{}", texts[0]);
    assert!(texts[1].ends_with("Extracted text:\nGamma"), "{}", texts[1]);
    assert_eq!(texts[2], prompts::IMAGE_PROMPT);
    assert!(d["content"].as_str().unwrap().contains("# Scanned page"));
}

#[tokio::test]
async fn a_runaway_or_an_empty_answer_is_a_failed_page_and_never_half_a_page() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let (_state, gw) = gateway(&m).await;
    m.will_answer("half a page of text that never ended", "length");
    let (status, a) = convert(&gw, &request(&[Some("Page one"), None], json!({}))).await;
    assert_eq!(status, 200);
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    assert!(!d["content"].as_str().unwrap().contains("half a page"));
    let failed = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == "part_failed")
        .expect("the failed page is named");
    assert_eq!(failed["parts"], json!([2]));
    let message = failed["message"].as_str().unwrap();
    assert!(
        message.contains("ran to the token limit after 300 tokens"),
        "{message}"
    );
    assert!(!message.contains("half a page"));
    assert_eq!(d["stats"]["parts_by_source"]["vision"], 0);
    assert_eq!(d["stats"]["model_calls"], 1);
    assert_eq!(d["stats"]["models"][0]["parts_read"], 0);

    // A page with text that the model fails keeps the tools' text.
    m.will_answer("  \n", "stop");
    let body = request(&[Some("Page one")], json!({"model_help": "every_page"}));
    let (_, a) = convert(&gw, &body).await;
    let d = &a["documents"][0];
    assert_eq!(d["content"], "Page one");
    assert_eq!(codes(d), [("part_failed".to_string(), vec![1])]);
    assert_eq!(d["stats"]["parts_by_source"]["text_layer"], 1);
}

const DOCTAGS: &str = "<doctag><title><loc_30><loc_50><loc_300><loc_100>Scanned title</title>\n\
<text><loc_30><loc_125><loc_470><loc_200>Body of the page.</text>\n\
<otsl><loc_30><loc_250><loc_470><loc_400><ched>Item<ched>Cost<nl><fcel>Pens<fcel>3<nl></otsl>\n\
</doctag>";

#[tokio::test]
async fn a_doctags_model_is_asked_docling_s_way_and_read_with_boxes() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let (state, gw) = gateway(&m).await;
    settings(&state, |s| s.documents.page_reader_doctags = true).await;
    let body = request(&[None], json!({"output": "json"}));

    m.will_answer(DOCTAGS, "stop");
    let (_, a) = convert(&gw, &body).await;
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert_eq!(d["warnings"], json!([]));
    let blocks = d["structure"][0]["blocks"].as_array().unwrap();
    let kinds: Vec<&str> = blocks.iter().map(|b| b["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["heading", "paragraph", "table"]);
    // The 500-step grid on the 300 × 200 pt page.
    assert_eq!(blocks[0]["bbox"], json!([18.0, 20.0, 180.0, 40.0]));
    assert_eq!(blocks[2]["table"]["cells"][0]["header"], true);
    assert_eq!(sent(&m.bodies().await[0]).0, prompts::DOCTAGS_PROMPT);

    // Cut in the middle of an element, but ended by the model itself: the
    // text is kept, and the page says what was wrong.
    m.will_answer(
        "<doctag><text><loc_1><loc_2><loc_3><loc_4>Kept words",
        "stop",
    );
    let (_, a) = convert(&gw, &body).await;
    let d = &a["documents"][0];
    assert_eq!(d["structure"][0]["blocks"][0]["text"], "Kept words");
    assert_eq!(codes(d), [("model_answer_malformed".to_string(), vec![1])]);

    // The tags lost on the way (a llama-server without --special).
    m.will_answer("Scanned title\n\nBody of the page.", "stop");
    let (_, a) = convert(&gw, &body).await;
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted");
    assert_eq!(d["structure"][0]["blocks"][1]["text"], "Body of the page.");
    let w = &d["warnings"][0];
    assert_eq!(w["code"], "model_answer_malformed");
    assert!(w["message"].as_str().unwrap().contains("--special"), "{w}");

    // A DocTags runaway is a failed page like any other.
    m.will_answer(
        &format!("<doctag>{}", "<text>again</text>".repeat(50)),
        "length",
    );
    let (status, a) = convert(&gw, &body).await;
    let d = &a["documents"][0];
    assert_eq!(d["status"], "failed", "{d}");
    // The file's error says why the model did not read its only page, with
    // the model's code and status (S6), and keeps the call that was paid.
    assert_eq!((status, &d["error"]["code"]), (502, &json!("model_failed")));
    let message = d["error"]["message"].as_str().unwrap();
    assert!(message.contains("ran to the token limit"), "{message}");
    assert!(!message.contains("again"), "{message}");
    assert!(!message.contains("not read as text yet"), "{message}");
    assert_eq!(d["stats"]["model_calls"], 1, "{d}");
    assert_eq!(d["stats"]["models"][0]["alias"], ALIAS);
    assert_eq!(d["stats"]["models"][0]["parts_read"], 0);
}

#[tokio::test]
async fn without_a_chosen_model_nothing_is_asked_and_the_request_is_told() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let state = state_with(&m, false).await;
    let gw = serve(state.clone()).await;
    let body = request(
        &[Some("Page one"), None],
        json!({"model_help": "every_page"}),
    );
    let (status, a) = convert(&gw, &body).await;
    assert_eq!(status, 200);
    assert_eq!(a["warnings"][0]["code"], "model_not_chosen");
    assert_eq!(a["warnings"][0]["option"], "model_help");
    assert_eq!(a["documents"][0]["stats"]["model_calls"], 0);
    assert!(m.bodies().await.is_empty(), "the model was never called");
    // The save refuses a name nothing serves, and takes the alias.
    let set = |v: Value| {
        let gw = gw.clone();
        async move {
            let r = gw
                .client()
                .post(format!("{gw}/api/op/settings_set"))
                .json(&v)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap_or_default())
        }
    };
    let (status, text) = set(json!({"documents_page_reader": "example-missing"})).await;
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("example-missing"), "{text}");
    let (status, text) = set(json!({"documents_page_reader": ALIAS})).await;
    assert_eq!(status, 200, "{text}");
    let r = gw
        .client()
        .post(format!("{gw}/api/op/doc_tools"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let caps: Value = r.json().await.unwrap();
    assert_eq!(caps["page_reader"]["alias"], ALIAS);
    assert_eq!(caps["page_reader"]["local"], false);
    assert_eq!(caps["page_reader"]["problems"], json!([]));
    // S4: the row is told where pages go: the upstream's name and host.
    assert_eq!(
        caps["page_reader"]["outside"],
        json!({"service": "example-up", "host": "127.0.0.1"})
    );
    assert!(caps["page_reader"].get("fallback").is_none(), "{caps}");
    // The page's own row for scans says who reads them now.
    let scans = caps["formats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "scans")
        .unwrap();
    // Without tesseract the model is the scans' reader; with it, it comes
    // after text recognition.
    if scans["handler"].is_null() {
        assert_eq!(scans["quality"], "partial", "{scans}");
    } else {
        assert_eq!(scans["handler"], "ocr.tesseract", "{scans}");
        assert!(
            scans["said"]
                .as_str()
                .unwrap()
                .contains("text recognition left unread or read poorly"),
            "{scans}"
        );
    }
    assert!(scans["said"].as_str().unwrap().contains(ALIAS), "{scans}");
}
