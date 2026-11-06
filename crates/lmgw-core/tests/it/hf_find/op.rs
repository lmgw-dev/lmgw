//! `model_find` and `lmgw__model_find` against a mock hub (find-a-model
//! design §10's integration list, WP4): a card with size marks and no
//! header read, the hub's rate limit (a 429, a window running out, and one
//! used up partway through the walk), every upload gated, an upload the hub
//! can't answer for, an upload with no file in the band, no route to the
//! hub, the token on the hub's reads only when set, a private repo without a
//! share link, the addresses (a file, a file not on `main`, a file that
//! isn't a model's, a sub-4-bit file, a hub page that isn't a model's, an
//! address elsewhere sending nothing), the cursor, the lists' windows, the
//! hub's gzip, and the tool.
//!
//! The names are fake and nothing here reaches the network. Every test
//! sets `HF_ENDPOINT`, so each holds `common::process_env_lock`.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use lmgw_api_types::model_find::{FindAnswer, FitMark, FitMeasure, ModelFind};
use lmgw_core::config::Settings;
use lmgw_core::find::card::Inputs;
use lmgw_core::hf::rate::Engine;
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::common::serve;
use crate::support::device_pair::op;

const GIB: u64 = 1 << 30;
const ORIGIN: &str = "maker-demo/Demo-2-8B-it";
const UPLOAD: &str = "fan-demo/Demo-2-8B-it-GGUF";
/// A second plain upload, liked less: the walk's next.
const UPLOAD2: &str = "other-fan-demo/Demo-2-8B-it-GGUF";
/// The maker's other original, liked more and too big for any card here.
const BIG: &str = "maker-demo/Demo-2-400B-it";

/// Inputs without a card, a rate or an engine: for the pure answer.
pub fn no_card_inputs() -> Inputs {
    Inputs {
        card: None,
        downloads_rate: None,
        engine: Engine::NotKnown("a test".into()),
        disk_free: None,
        token_set: false,
    }
}

/// What the mock hub serves, and how.
#[derive(Clone)]
struct Demo {
    upload_gated: bool,
    upload_private: bool,
    /// The search answers this status, with this `RateLimit`.
    search_status: u16,
    ratelimit: String,
    /// The first search's next page.
    next: bool,
    /// Every answer gzipped, as the live hub sends it when asked (D141).
    gzip: bool,
    /// What the first upload's tree answers instead of its files.
    tree_status: Option<u16>,
    /// The first upload holds only files outside the band (Q2_K, F16).
    narrow_only: bool,
    /// [`UPLOAD2`] is listed too.
    second: bool,
    /// [`BIG`] is in the search.
    big: bool,
    /// From this request on (counted from 1), `RateLimit` says none is left.
    spent_from: Option<usize>,
    /// Originals of other authors in the search, for the lists' windows.
    extra: usize,
    asked: Arc<AtomicUsize>,
}

impl Default for Demo {
    fn default() -> Self {
        Self {
            upload_gated: false,
            upload_private: false,
            search_status: 200,
            ratelimit: "\"api\";r=495;t=200".into(),
            next: false,
            gzip: false,
            tree_status: None,
            narrow_only: false,
            second: false,
            big: false,
            spent_from: None,
            extra: 0,
            asked: Arc::default(),
        }
    }
}

fn gz(v: &Value) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(&serde_json::to_vec(v).unwrap()).unwrap();
    e.finish().unwrap()
}

/// The usual files of an upload of the 8 B model.
fn files() -> Value {
    json!([
        {"type": "file", "path": "Demo-2-8B-it-Q2_K.gguf", "size": 3 * GIB},
        {"type": "file", "path": "Demo-2-8B-it-Q4_K_M.gguf", "size": 5 * GIB},
        {"type": "file", "path": "Demo-2-8B-it-Q8_0.gguf", "size": 9 * GIB},
        {"type": "file", "path": "Demo-2-8B-it-F16.gguf", "size": 16 * GIB},
        {"type": "file", "path": "mmproj-Demo-2-8B-it-F16.gguf", "size": GIB / 2},
        {"type": "file", "path": "README.md", "size": 100},
    ])
}

fn original() -> Value {
    json!({
        "id": ORIGIN, "author": "maker-demo", "likes": 500, "downloads": 9000,
        "gated": false, "private": false, "pipeline_tag": "text-generation",
        "tags": ["safetensors", "conversational", "license:apache-2.0"],
        "safetensors": {"total": 8_000_000_000u64},
        "baseModels": {"relation": "finetune", "models": [{"id": "maker-demo/Demo-2-8B"}]}
    })
}

impl Demo {
    fn upload(&self) -> Value {
        json!({
            "id": UPLOAD, "author": "fan-demo", "likes": 50, "downloads": 70000,
            "gated": if self.upload_gated { json!("manual") } else { json!(false) },
            "private": self.upload_private, "tags": ["gguf"], "pipeline_tag": "text-generation",
            "baseModels": {"relation": "quantized", "models": [{"id": ORIGIN}]}
        })
    }

    fn second(&self) -> Value {
        json!({
            "id": UPLOAD2, "author": "other-fan-demo", "likes": 10, "downloads": 900,
            "gated": false, "private": false, "tags": ["gguf"],
            "baseModels": {"relation": "quantized", "models": [{"id": ORIGIN}]}
        })
    }

    fn answer(&self, r: &Request) -> ResponseTemplate {
        let n = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
        let path = r.url.path();
        let q: Vec<(String, String)> = r.url.query_pairs().into_owned().collect();
        let has = |k: &str, v: &str| q.iter().any(|(a, b)| a == k && b == v);
        let spent = self.spent_from.is_some_and(|k| n >= k);
        let with_rl = |t: ResponseTemplate| {
            let rl = match spent {
                true => "\"api\";r=0;t=120",
                false => self.ratelimit.as_str(),
            };
            t.insert_header("ratelimit", rl)
                .insert_header("ratelimit-policy", "\"fixed window\";\"api\";q=500;w=300")
        };
        let json_ok = |v: Value| {
            with_rl(match self.gzip {
                true => ResponseTemplate::new(200)
                    .set_body_raw(gz(&v), "application/json")
                    .insert_header("content-encoding", "gzip"),
                false => ResponseTemplate::new(200).set_body_json(v),
            })
        };
        match path {
            "/api/models" if q.iter().any(|(k, _)| k == "search") => {
                if self.search_status != 200 {
                    return with_rl(ResponseTemplate::new(self.search_status));
                }
                let cursor = q.iter().any(|(k, _)| k == "cursor");
                let gguf = has("filter", "gguf");
                let mut body = match (gguf, cursor) {
                    (_, true) => json!([{"id": "other-demo/Demo-2-tiny", "author": "other-demo",
                                         "likes": 1, "safetensors": {"total": 1000}}]),
                    (false, false) => json!([original(), {"id": "someone-demo/Other-2",
                                             "author": "someone-demo", "likes": 3}]),
                    (true, false) => json!([self.upload()]),
                };
                if !gguf && !cursor {
                    let list = body.as_array_mut().unwrap();
                    if self.big {
                        list.push(json!({
                            "id": BIG, "author": "maker-demo", "likes": 900, "gated": false,
                            "pipeline_tag": "text-generation", "tags": ["conversational"],
                            "safetensors": {"total": 400_000_000_000u64}
                        }));
                    }
                    for i in 0..self.extra {
                        list.push(json!({
                            "id": format!("author{i}-demo/Demo-2-{i}B"),
                            "author": format!("author{i}-demo"), "likes": 1 + i,
                            "safetensors": {"total": 1_000_000_000u64}
                        }));
                    }
                }
                let mut t = json_ok(body);
                if self.next && !gguf && !cursor {
                    t = t.insert_header(
                        "link",
                        "<https://huggingface.co/api/models?search=demo+2&cursor=page2>; rel=\"next\"",
                    );
                }
                t
            }
            "/api/models" if has("filter", &format!("base_model:quantized:{ORIGIN}")) => {
                match self.second {
                    true => json_ok(json!([self.upload(), self.second()])),
                    false => json_ok(json!([self.upload()])),
                }
            }
            p if p == format!("/api/models/{UPLOAD}/tree/main") => {
                if self.upload_gated {
                    return with_rl(ResponseTemplate::new(401));
                }
                if let Some(status) = self.tree_status {
                    return with_rl(ResponseTemplate::new(status));
                }
                match self.narrow_only {
                    true => json_ok(json!([
                        {"type": "file", "path": "Demo-2-8B-it-Q2_K.gguf", "size": 3 * GIB},
                        {"type": "file", "path": "Demo-2-8B-it-F16.gguf", "size": 16 * GIB},
                    ])),
                    false => json_ok(files()),
                }
            }
            p if p == format!("/api/models/{UPLOAD2}/tree/main") => json_ok(files()),
            // The original's own repo: its config and safetensors, no GGUF.
            p if p == format!("/api/models/{ORIGIN}/tree/main") => json_ok(json!([
                {"type": "file", "path": "config.json", "size": 900},
                {"type": "file", "path": "model.safetensors", "size": 16 * GIB},
            ])),
            p if p == format!("/api/models/{UPLOAD}") => json_ok(self.upload()),
            p if p == format!("/api/models/{ORIGIN}") => json_ok(original()),
            "/api/organizations/maker-demo/overview" => with_rl(ResponseTemplate::new(404)),
            "/api/users/maker-demo/overview" => json_ok(json!({"fullname": "Maker Demo"})),
            _ => with_rl(ResponseTemplate::new(404)),
        }
    }
}

/// A mock hub serving `demo`, set as `HF_ENDPOINT`.
async fn hub(demo: Demo) -> MockServer {
    let hub = MockServer::start().await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(move |r: &Request| demo.answer(r))
        .mount(&hub)
        .await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    hub
}

/// A state with a declared 24 GiB card and 1 GiB headroom, and `token`.
async fn state(token: &str) -> SharedState {
    state_with(token, 24).await
}

/// A state with a declared card of `gib`, 1 GiB headroom, and `token`.
pub(super) async fn state_with(token: &str, gib: u64) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = std::env::temp_dir();
    let mut settings = Settings::default();
    settings.router.models_dir = dir.display().to_string();
    settings.aux_router.models_dir = dir.display().to_string();
    settings.vram.budget_mb = gib * 1024;
    settings.vram.headroom_mb = 1024;
    settings.hf_token = token.to_string();
    store::save_settings(&state.db, &settings).await.unwrap();
    state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry("none".into())));
    state.reload_snapshot().await.unwrap();
    state
}

pub(super) async fn find(state: &SharedState, args: Value) -> ModelFind {
    let gw = serve(state.clone()).await;
    let (status, body) = op(&gw, "model_find", args).await;
    assert_eq!(status, 200, "{body}");
    crate::common::round_trips::<ModelFind>("model_find", &body)
}

/// The whole card by size: the original, the maker's display name, the
/// widest file in the band that fits, its projector, the licence, the share
/// link on the public hub, the hub's budget, and no file header read.
#[tokio::test]
async fn a_card_by_size_reads_no_header() {
    let _env = crate::common::process_env_lock().await;
    let hub = hub(Demo::default()).await;
    let state = state("").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::Card, "{found:?}");
    let card = found.card.unwrap();
    assert_eq!(card.origin, ORIGIN);
    assert_eq!(card.name, "Demo 2 8B it");
    assert_eq!(card.by, "Maker Demo");
    // A person's account (the users overview answered): the id rides along.
    assert_eq!(card.by_user.as_deref(), Some("maker-demo"));
    assert_eq!(found.sentence, "Demo 2 8B it by Maker Demo (maker-demo).");
    assert_eq!(card.upload, UPLOAD);
    // 8 B parameters: F16 is 17 bits, past the Q8 class; Q8_0 is 9.7.
    assert_eq!(card.file, "Demo-2-8B-it-Q8_0.gguf");
    assert_eq!(card.fit.mark, FitMark::FitsBySize);
    assert_eq!(card.fit.measure, FitMeasure::Beside);
    assert!(card.whole_card.is_none(), "{card:?}");
    assert_eq!(card.companions.len(), 1);
    assert_eq!(card.companions[0].file, "mmproj-Demo-2-8B-it-F16.gguf");
    assert_eq!(card.can, ["reads pictures"]);
    assert_eq!(card.download.files_bytes, 9 * GIB + GIB / 2);
    assert_eq!(card.parameters, Some(8_000_000_000));
    let licence = card.licence.unwrap();
    assert_eq!(licence.id.as_deref(), Some("apache-2.0"));
    assert_eq!(
        licence.link.as_deref(),
        Some("https://huggingface.co/maker-demo/Demo-2-8B-it")
    );
    let share = card.share.unwrap();
    assert_eq!(
        share.url,
        format!("https://huggingface.co/{UPLOAD}/blob/main/Demo-2-8B-it-Q8_0.gguf")
    );
    assert!(
        share.text.starts_with("Get Demo 2 8B it in lmgw"),
        "{share:?}"
    );
    assert!(!share.text.contains("127.0.0.1"), "{share:?}");
    // The other originals and the partial match.
    assert!(found.others.is_empty(), "{:?}", found.others);
    assert_eq!(found.did_you_mean.len(), 1);
    assert_eq!(found.did_you_mean[0].origin, "someone-demo/Other-2");
    // The hub's own figures, after six reads.
    let budget = found.hub_budget.unwrap();
    assert_eq!(budget.remaining, Some(495));
    assert_eq!((budget.quota, budget.window_s), (Some(500), Some(300)));
    assert_eq!(budget.calls, 6, "{budget:?}");
    assert!(!budget.near);
    let asked = hub.received_requests().await.unwrap();
    assert!(asked.iter().all(|r| !r.url.path().contains("/resolve/")));
    // No token configured: none sent.
    assert!(asked
        .iter()
        .all(|r| !r.headers.contains_key("authorization")));
}

/// A configured token rides every hub read (D2, D124); the answer is the
/// same.
#[tokio::test]
async fn a_configured_token_rides_the_hubs_reads() {
    let _env = crate::common::process_env_lock().await;
    let hub = hub(Demo::default()).await;
    let state = state("hf_demo_token").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::Card);
    let asked = hub.received_requests().await.unwrap();
    assert!(!asked.is_empty());
    for r in asked {
        assert_eq!(
            r.headers.get("authorization").unwrap(),
            "Bearer hf_demo_token",
            "{}",
            r.url
        );
    }
}

/// A 429 answers `rate_limited` with the hub's reset and its policy's quota,
/// in words; a window with fewer requests left than a search took is `near`.
#[tokio::test]
async fn the_hubs_rate_limit_is_said() {
    let _env = crate::common::process_env_lock().await;
    let _hub = hub(Demo {
        search_status: 429,
        ratelimit: "\"api\";r=0;t=95".into(),
        ..Demo::default()
    })
    .await;
    let state = state("").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::RateLimited);
    let budget = found.hub_budget.unwrap();
    assert_eq!((budget.remaining, budget.reset_s), (Some(0), Some(95)));
    assert_eq!(budget.calls, 1, "nothing asked after the 429");
    assert_eq!(
        found.sentence,
        "Hugging Face asks lmgw to wait about 2 minutes. Without an account it allows 500 \
         requests every 5 minutes; a Hugging Face token in Settings raises that."
    );

    let _hub = hub(Demo {
        ratelimit: "\"api\";r=2;t=40".into(),
        ..Demo::default()
    })
    .await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::Card);
    let budget = found.hub_budget.unwrap();
    assert!(budget.near, "{budget:?}");
    assert!(budget
        .sentence
        .unwrap()
        .starts_with("Hugging Face's limit resets in 40 seconds: 2 requests are left"),);
}

/// Every upload gated, its tree refused: `all_gated`, naming the repo.
#[tokio::test]
async fn every_upload_gated() {
    let _env = crate::common::process_env_lock().await;
    let _hub = hub(Demo {
        upload_gated: true,
        ..Demo::default()
    })
    .await;
    let state = state("").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::AllGated, "{found:?}");
    assert!(found.card.is_none());
    assert!(
        found.sentence.contains(&format!("huggingface.co/{UPLOAD}")),
        "{}",
        found.sentence
    );
}

/// A private upload's card has no share link (D124).
#[tokio::test]
async fn a_private_repo_has_no_share_link() {
    let _env = crate::common::process_env_lock().await;
    let _hub = hub(Demo {
        upload_private: true,
        ..Demo::default()
    })
    .await;
    let state = state("hf_demo_token").await;
    let card = find(&state, json!({"query": "demo 2"})).await.card.unwrap();
    assert!(card.private);
    assert!(card.share.is_none());
}

/// No route to the hub: `unreachable`, the reason in the notes.
#[tokio::test]
async fn no_route_to_the_hub() {
    let _env = crate::common::process_env_lock().await;
    let (port, _socket) = crate::common::refusing_port();
    std::env::set_var("HF_ENDPOINT", format!("http://127.0.0.1:{port}"));
    let state = state("").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::Unreachable);
    assert_eq!(
        found.sentence,
        "lmgw can't reach Hugging Face right now. Check the internet connection and try again."
    );
    assert!(!found.notes.is_empty());
}

/// An address on another host is answered here and nothing is sent; an
/// empty query is refused.
#[tokio::test]
async fn an_address_elsewhere_sends_nothing() {
    let _env = crate::common::process_env_lock().await;
    let hub = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&hub)
        .await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    let state = state("hf_demo_token").await;
    let found = find(
        &state,
        json!({"query": "try https://models.example/demo/Demo-2"}),
    )
    .await;
    assert_eq!(found.answer, FindAnswer::NotHuggingFace);
    assert_eq!(
        found.sentence,
        "lmgw finds models on Hugging Face. This address is on models.example."
    );
    assert!(found.hub_budget.is_none());
    let gw = serve(state.clone()).await;
    let (status, body) = op(&gw, "model_find", json!({"query": "  "})).await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = op(&gw, "model_find", json!({"qurey": "x"})).await;
    assert_eq!(status, 400, "{body}");
}

/// A pasted file address: the upload's own listing, its original's, its
/// tree, and that file as the card's pick because it fits.
#[tokio::test]
async fn an_address_names_the_file() {
    let _env = crate::common::process_env_lock().await;
    let _hub = hub(Demo::default()).await;
    let state = state("").await;
    let text = format!(
        "Get Demo 2 8B it in lmgw: open Models \u{2192} Find a model and paste\n\
         https://huggingface.co/{UPLOAD}/blob/main/Demo-2-8B-it-Q4_K_M.gguf"
    );
    let found = find(&state, json!({ "query": text })).await;
    assert_eq!(found.answer, FindAnswer::Card, "{found:?}");
    let card = found.card.unwrap();
    assert_eq!(card.origin, ORIGIN);
    assert_eq!(card.upload, UPLOAD);
    assert_eq!(card.file, "Demo-2-8B-it-Q4_K_M.gguf");
    assert!(
        card.why.contains("the file the address names"),
        "{}",
        card.why
    );
    assert_eq!(card.named_file.as_deref(), Some("Demo-2-8B-it-Q4_K_M.gguf"));
}

/// WP7, the friend's side of "Send to a friend": the share text as the
/// Find box gets it (the sentence with its line break lost, as a
/// single-line input drops it and glues the address to the word before, or
/// the address alone) plans that exact file when it fits. When it doesn't,
/// the card says so and shows the same upload's file that does (§7, D100).
#[tokio::test]
async fn a_pasted_share_text_plans_that_file_or_says_it_doesnt_fit() {
    let _env = crate::common::process_env_lock().await;
    let _hub = hub(Demo::default()).await;
    let address = format!("https://huggingface.co/{UPLOAD}/blob/main/Demo-2-8B-it-Q4_K_M.gguf");
    let state = state("").await;
    let sentence = "Get Demo 2 8B it in lmgw: open Models \u{2192} Find a model and paste";
    for pasted in [
        format!("{sentence}\n{address}"),
        format!("{sentence}{address}"),
        address.clone(),
        address.replace("https://", ""),
    ] {
        let found = find(&state, json!({ "query": pasted })).await;
        assert_eq!(found.answer, FindAnswer::Card, "{pasted}: {found:?}");
        let card = found.card.unwrap();
        assert_eq!(card.upload, UPLOAD, "{pasted}");
        assert_eq!(card.file, "Demo-2-8B-it-Q4_K_M.gguf", "{pasted}");
        assert_eq!(card.named_file.as_deref(), Some("Demo-2-8B-it-Q4_K_M.gguf"));
        // The card's own share link names the same file.
        assert_eq!(card.share.unwrap().url, address);
    }
    let found = find(&state, json!({ "query": ORIGIN })).await;
    assert_eq!(
        found.card.unwrap().named_file,
        None,
        "no address, no named file"
    );
    // The friend's card is 8 GiB: the 9.5 GiB Q8_0 the address names doesn't
    // fit it with the headroom, so the card shows the Q4_K_M of the upload.
    let small = state_with("", 8).await;
    let big = format!("https://huggingface.co/{UPLOAD}/blob/main/Demo-2-8B-it-Q8_0.gguf");
    let found = find(&small, json!({ "query": format!("{sentence}\n{big}") })).await;
    let card = found.card.unwrap();
    assert_eq!(card.upload, UPLOAD);
    assert_eq!(card.file, "Demo-2-8B-it-Q4_K_M.gguf");
    assert_eq!(card.named_file, None);
    assert!(
        card.fit.notes.iter().any(|n| n
            .contains("Demo-2-8B-it-Q8_0.gguf, the file the address names")
            && n.contains("shows the file of the same upload that fits")),
        "{:?}",
        card.fit.notes
    );
}

/// No match at all: the digits hint (D18).
#[tokio::test]
async fn no_match_asks_for_digits() {
    let _env = crate::common::process_env_lock().await;
    let _hub = hub(Demo::default()).await;
    let state = state("").await;
    let found = find(&state, json!({"query": "zzz"})).await;
    assert_eq!(found.answer, FindAnswer::NoMatch);
    assert_eq!(
        found.sentence,
        "lmgw found no model called \u{201c}zzz\u{201d} that it can run. Names are usually \
         written as on the model's page, with numbers as digits."
    );
}

/// The hub's next page comes back as `next_cursor`, and the cursor asks for
/// it: answer `more`, the lists only.
#[tokio::test]
async fn the_cursor_follows_the_hubs_next_page() {
    let _env = crate::common::process_env_lock().await;
    let hub = hub(Demo {
        next: true,
        ..Demo::default()
    })
    .await;
    let state = state("").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    let cursor = found.next_cursor.expect("a next page");
    let more = find(&state, json!({"query": "demo 2", "cursor": cursor})).await;
    assert_eq!(more.answer, FindAnswer::More);
    assert!(more.card.is_none());
    assert_eq!(more.others.len(), 1, "{more:?}");
    let asked = hub.received_requests().await.unwrap();
    let last = asked.last().unwrap();
    assert!(
        last.url.query().unwrap().contains("cursor=page2"),
        "{}",
        last.url
    );
    // Only the search with a next page was asked again.
    assert!(
        !last.url.query().unwrap().contains("filter=gguf"),
        "{}",
        last.url
    );
    let gw = serve(state.clone()).await;
    let (status, _) = op(
        &gw,
        "model_find",
        json!({"query": "demo 2", "cursor": "nonsense"}),
    )
    .await;
    assert_eq!(status, 400);
}

/// `lmgw__model_find` is a read of the same answer, refusing an argument it
/// doesn't take.
#[tokio::test]
async fn the_tool_is_a_read_of_the_same_answer() {
    let _env = crate::common::process_env_lock().await;
    let ro = lmgw_core::mcp::selfadmin::list(lmgw_core::config::SelfAdmin::ReadOnly);
    assert!(ro.iter().any(|t| t["name"] == "lmgw__model_find"));
    assert!(!lmgw_core::mcp::selfadmin::writes("lmgw__model_find"));
    let _hub = hub(Demo::default()).await;
    let state = state("").await;
    let admin = crate::support::self_admin::Admin::new(&state).await;
    let text = admin
        .text("lmgw__model_find", json!({"query": "demo 2"}))
        .await;
    let found: ModelFind = crate::support::self_admin::read_back(&text);
    assert_eq!(found.card.unwrap().origin, ORIGIN);
    let (failed, text) = admin
        .call("lmgw__model_find", json!({"query": "demo 2", "limit": 3}))
        .await;
    assert!(
        failed && text.contains("unknown argument 'limit'"),
        "{text}"
    );
}

/// The hub's limit runs out partway through the walk (D76): its last answer
/// said none are left, so nothing more is sent, and what was found stays:
/// the maker's model passed over by its size as the `nothing_fits` card,
/// the lists, and a note naming the limit and its reset.
#[tokio::test]
async fn a_limit_used_up_partway_keeps_what_was_found() {
    let _env = crate::common::process_env_lock().await;
    // 1 and 2 are the searches, 3 the 8 B model's uploads: its answer says
    // no request is left.
    let server = hub(Demo {
        big: true,
        spent_from: Some(3),
        ..Demo::default()
    })
    .await;
    let state = state("").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    assert_eq!(found.answer, FindAnswer::NothingFits, "{found:?}");
    let card = found.card.as_ref().unwrap();
    assert_eq!(card.origin, BIG);
    assert!(card.file.is_empty(), "{card:?}");
    assert!(
        found.sentence.starts_with(
            "Of the models by maker-demo with this name that lmgw could check, Demo 2 400B it"
        ),
        "{}",
        found.sentence
    );
    assert!(
        found.notes.iter().any(|n| n.starts_with(
            "Hugging Face's limit on requests ran out partway: lmgw checked 1 of the 2 models"
        ) && n.ends_with("It resets in about 2 minutes.")),
        "{:?}",
        found.notes
    );
    assert_eq!(found.others_total, 1);
    assert_eq!(found.others[0].origin, ORIGIN);
    let budget = found.hub_budget.unwrap();
    assert_eq!(budget.remaining, Some(0));
    assert!(budget.near);

    // Spent before anything was found: `rate_limited`, the lists kept.
    let server = hub(Demo {
        spent_from: Some(3),
        ..Demo::default()
    })
    .await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    assert_eq!(found.answer, FindAnswer::RateLimited, "{found:?}");
    assert!(found.card.is_none());
    assert_eq!((found.others_total, found.did_you_mean_total), (1, 1));
    assert_eq!(found.others[0].origin, ORIGIN);
}

/// An upload the hub answers 404 for is passed for the next (finding 4);
/// when it was the only one, the answer says the search was answered and
/// the files weren't, not that the hub is out of reach.
#[tokio::test]
async fn an_upload_the_hub_cant_answer_for_is_passed() {
    let _env = crate::common::process_env_lock().await;
    let _hub = hub(Demo {
        tree_status: Some(404),
        second: true,
        ..Demo::default()
    })
    .await;
    let state = state("").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::Card, "{found:?}");
    assert_eq!(found.card.unwrap().upload, UPLOAD2);
    assert!(
        found.notes.iter().any(|n| n.contains(&format!(
            "Hugging Face answered 404 when lmgw asked for the files of {UPLOAD}"
        ))),
        "{:?}",
        found.notes
    );

    let _hub = hub(Demo {
        tree_status: Some(503),
        ..Demo::default()
    })
    .await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::Unreachable, "{found:?}");
    assert!(
        found
            .sentence
            .starts_with("Hugging Face answered, but not when lmgw asked"),
        "{}",
        found.sentence
    );
}

/// An upload whose files are all outside the band is passed like one
/// without weights (finding 2); when no upload has a file in the band, the
/// answer says what the files are.
#[tokio::test]
async fn an_upload_with_no_file_in_the_band_is_passed() {
    let _env = crate::common::process_env_lock().await;
    let _hub = hub(Demo {
        narrow_only: true,
        second: true,
        ..Demo::default()
    })
    .await;
    let state = state("").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::Card, "{found:?}");
    let card = found.card.unwrap();
    assert_eq!(card.upload, UPLOAD2);
    assert_eq!(card.file, "Demo-2-8B-it-Q8_0.gguf");
    assert!(
        found.notes.iter().any(|n| n.contains(&format!(
            "lmgw picks no file of {UPLOAD} by itself: every file is narrower than 4 bits per \
             weight"
        ))),
        "{:?}",
        found.notes
    );

    let _hub = hub(Demo {
        narrow_only: true,
        ..Demo::default()
    })
    .await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::NoFileInBand, "{found:?}");
    assert!(found.card.is_none());
    assert!(
        found.sentence.contains(
            "every file is narrower than 4 bits per weight, which loses too much of the model, \
             or the full 16- or 32-bit weights"
        ) && found.sentence.contains("under 12.25 bits per weight")
            && found
                .sentence
                .ends_with(&format!("huggingface.co/{UPLOAD}.")),
        "{}",
        found.sentence
    );
}

/// The addresses of §4.1 that name a file (findings 1, 6, 8): in a Markdown
/// link; a file of the original's own repo that isn't a GGUF; a file not on
/// `main`; a sub-4-bit file that fits where the band's smallest doesn't.
#[tokio::test]
async fn the_addresses_that_name_a_file() {
    let _env = crate::common::process_env_lock().await;
    let _hub = hub(Demo::default()).await;
    let state = state("").await;
    let card = |f: &ModelFind| f.card.clone().unwrap_or_else(|| panic!("{f:?}"));

    let found = find(
        &state,
        json!({"query": format!(
            "[the file](https://huggingface.co/{UPLOAD}/blob/main/Demo-2-8B-it-Q4_K_M.gguf)"
        )}),
    )
    .await;
    assert_eq!(card(&found).file, "Demo-2-8B-it-Q4_K_M.gguf");

    for named in ["blob/main/config.json", "resolve/main/model.safetensors"] {
        let found = find(
            &state,
            json!({"query": format!("https://huggingface.co/{ORIGIN}/{named}")}),
        )
        .await;
        assert_eq!(found.answer, FindAnswer::Card, "{named}: {found:?}");
        assert_eq!(card(&found).upload, UPLOAD, "{named}");
        assert!(
            found
                .notes
                .iter()
                .any(|n| n.contains("isn't one lmgw can run: lmgw runs GGUF files")),
            "{named}: {:?}",
            found.notes
        );
    }

    let found = find(
        &state,
        json!({"query": format!(
            "https://huggingface.co/{UPLOAD}/blob/v1/Demo-2-8B-it-Q5_K_M.gguf"
        )}),
    )
    .await;
    assert_eq!(card(&found).file, "Demo-2-8B-it-Q8_0.gguf");
    assert!(
        found.notes.iter().any(|n| n.contains(&format!(
            "isn't among the files of {UPLOAD} on its main branch"
        )) && n.contains("revision v1")),
        "{:?}",
        found.notes
    );

    // On a 5 GiB card the band's smallest (Q4_K_M, 5 GiB) doesn't fit with
    // the headroom; the Q2_K the address names does, and is the card's.
    let small = state_with("", 5).await;
    let found = find(
        &small,
        json!({"query": format!("https://huggingface.co/{UPLOAD}/blob/main/Demo-2-8B-it-Q2_K.gguf")}),
    )
    .await;
    assert_eq!(found.answer, FindAnswer::Card, "{found:?}");
    assert_eq!(card(&found).file, "Demo-2-8B-it-Q2_K.gguf");
    assert_eq!(card(&found).fit.mark, FitMark::FitsBySize);
}

/// A hub page that isn't a model's is answered here, nothing sent (finding
/// 7); the hub's own search page is its search's text.
#[tokio::test]
async fn hub_pages_that_arent_a_model_send_nothing() {
    let _env = crate::common::process_env_lock().await;
    let refusing = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&refusing)
        .await;
    std::env::set_var("HF_ENDPOINT", refusing.uri());
    let state = state("").await;
    for (query, starts) in [
        (
            "https://huggingface.co/maker-demo",
            "huggingface.co/maker-demo is the page of maker-demo on Hugging Face",
        ),
        (
            "https://huggingface.co/datasets/maker-demo/demo-set",
            "huggingface.co/datasets/maker-demo/demo-set is a dataset",
        ),
        (
            "https://huggingface.co/spaces/maker-demo/demo-app",
            "huggingface.co/spaces/maker-demo/demo-app is a Space",
        ),
    ] {
        let found = find(&state, json!({ "query": query })).await;
        assert_eq!(found.answer, FindAnswer::NotAModel, "{query}");
        assert!(found.sentence.starts_with(starts), "{}", found.sentence);
        assert!(found.hub_budget.is_none());
    }
    drop(refusing);
    let _hub = hub(Demo::default()).await;
    let found = find(
        &state,
        json!({"query": "https://huggingface.co/models?search=demo+2"}),
    )
    .await;
    assert_eq!(found.query, "https://huggingface.co/models?search=demo+2");
    assert_eq!(found.card.unwrap().origin, ORIGIN);
}

/// `others` and `did_you_mean` are windows (D142): ten by default, the
/// totals and what comes after always said, another window by its offset
/// and count, in the op and the tool alike.
#[tokio::test]
async fn the_lists_come_in_windows() {
    let _env = crate::common::process_env_lock().await;
    let _hub = hub(Demo {
        extra: 25,
        second: true,
        ..Demo::default()
    })
    .await;
    let state = state("").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::Card);
    let card = found.card.as_ref().unwrap();
    assert_eq!(card.other_uploads.len(), 1);
    assert_eq!((card.other_uploads_total, card.other_uploads_more), (1, 0));
    assert_eq!(
        (found.others.len(), found.others_total, found.others_more),
        (10, 25, 15)
    );
    // By likes: the most liked of the others first.
    assert_eq!(found.others[0].origin, "author24-demo/Demo-2-24B");
    assert_eq!((found.did_you_mean.len(), found.did_you_mean_total), (1, 1));
    let found = find(
        &state,
        json!({"query": "demo 2", "others_offset": 20, "others_count": 10,
               "did_you_mean_count": 0, "other_uploads_count": 0}),
    )
    .await;
    let card = found.card.as_ref().unwrap();
    assert!(card.other_uploads.is_empty());
    assert_eq!(card.other_uploads_more, 1);
    assert_eq!(
        (found.others.len(), found.others_more),
        (5, 0),
        "{:?}",
        found.others
    );
    assert_eq!(found.others[0].origin, "author4-demo/Demo-2-4B");
    assert!(found.did_you_mean.is_empty());
    assert_eq!(found.did_you_mean_more, 1);

    let admin = crate::support::self_admin::Admin::new(&state).await;
    let text = admin
        .text(
            "lmgw__model_find",
            json!({"query": "demo 2", "others_offset": 5, "others_count": 3}),
        )
        .await;
    let found: ModelFind = crate::support::self_admin::read_back(&text);
    assert_eq!(found.others.len(), 3);
    assert_eq!(found.others[0].origin, "author19-demo/Demo-2-19B");
    assert_eq!(found.others_more, 17);
    let (failed, text) = admin
        .call(
            "lmgw__model_find",
            json!({"query": "demo 2", "others_count": -1}),
        )
        .await;
    assert!(failed && text.contains("must be 0 or more"), "{text}");
}

/// The hub's answers come gzipped when asked, and decode (D141).
#[tokio::test]
async fn the_hubs_gzip_decodes() {
    let _env = crate::common::process_env_lock().await;
    let hub = hub(Demo {
        gzip: true,
        ..Demo::default()
    })
    .await;
    let state = state("").await;
    let found = find(&state, json!({"query": "demo 2"})).await;
    assert_eq!(found.answer, FindAnswer::Card, "{found:?}");
    assert_eq!(found.card.unwrap().file, "Demo-2-8B-it-Q8_0.gguf");
    let asked = hub.received_requests().await.unwrap();
    assert!(asked.iter().all(|r| r
        .headers
        .get("accept-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("gzip"))));
}

/// Every client but the hub's opts out of gzip: a request through
/// `state.http` (an upstream's, a file's) and through `proxy_http` carries no
/// `Accept-Encoding: gzip`, as before the feature was on (D141).
#[tokio::test]
async fn the_other_clients_ask_for_no_gzip() {
    let _env = crate::common::process_env_lock().await;
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    let state = state("").await;
    for client in [&state.http, &state.proxy_http] {
        client.get(server.uri()).send().await.unwrap();
    }
    state.hub_http.get(server.uri()).send().await.unwrap();
    let asked = server.received_requests().await.unwrap();
    assert_eq!(asked.len(), 3);
    let gzip = |r: &Request| {
        r.headers
            .get("accept-encoding")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("gzip"))
    };
    assert!(!gzip(&asked[0]) && !gzip(&asked[1]), "{asked:?}");
    assert!(gzip(&asked[2]), "the hub's client asks for gzip");
}
