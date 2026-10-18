//! The aux class with a row of each kind — the decision row `laya` among
//! them — on a local "container" that is a wiremock server, for the System
//! One suites (decision-models design §6.1: `aux_router.rs`, `systemone.rs`).
//!
//! As in `aux_router.rs`'s `aux_world`: a podman that starts nothing, and the
//! port allocator pointed at the mock, so the container a row's start brings
//! up "is" that server. This podman also keeps the verbs it was asked to run,
//! so a suite can say that a refusal started no container.
//!
//! Public names come from [`public_name`] only, never a literal prefix: the
//! decision kind gets a prefix of its own (§3.10), and that change is one line
//! here.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use lmgw_core::config::{AuxKind, Settings};
use lmgw_core::erased::Erased;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAuxModel};

use crate::common::{serve, Gw};

/// The decision row.
pub const DECISION: &str = "laya";
/// The embedding row.
pub const EMBED: &str = "bge-m3";
/// The rerank row.
pub const RERANK: &str = "bge-reranker-v2-m3";

/// A `podman` that agrees to everything, starts nothing, and keeps the verb
/// of every command (`run`, `rm`, `inspect`, …).
#[derive(Default)]
pub struct RecordingPodman(Mutex<Vec<String>>);

impl RecordingPodman {
    /// How many containers were started (`podman run`).
    pub fn runs(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|v| *v == "run")
            .count()
    }
}

impl CommandRunner for RecordingPodman {
    fn run<'a>(
        &'a self,
        _program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        Box::pin(async move {
            if let Some(verb) = args.first() {
                self.0.lock().unwrap().push(verb.clone());
            }
            Ok(CmdOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        })
    }
}

pub struct DecisionWorld {
    pub state: SharedState,
    pub gw: Gw,
    pub podman: Arc<RecordingPodman>,
}

impl DecisionWorld {
    /// The public name of the aux row `model_id`.
    pub fn name(&self, model_id: &str) -> String {
        public_name(&self.state, model_id)
    }
}

/// The public name of the aux row `model_id`: the one place a suite of the
/// decision kind spells one. Its kind picks the prefix (§3.10): `decide/`
/// for the decision row, `embed/` for the others.
pub fn public_name(state: &SharedState, model_id: &str) -> String {
    let snap = state.snapshot();
    let row = snap
        .aux_models
        .iter()
        .find(|m| m.model_id == model_id)
        .unwrap_or_else(|| panic!("no aux row '{model_id}'"));
    snap.aux_public_name(row)
}

/// Rows of all three kinds, their containers on `container`'s port
/// (`http://host:port`). A dead port fails a start after two seconds,
/// which is what makes "refused before anything started" visible.
pub async fn decision_world(container: &str) -> DecisionWorld {
    decision_world_on(AppState::init_for_tests().await.unwrap(), container).await
}

/// [`decision_world`] on `state`: a gateway on a database of the test's
/// choosing (a file, where WAL and the write lock behave as in production).
pub async fn decision_world_on(state: SharedState, container: &str) -> DecisionWorld {
    let port: u16 = container
        .rsplit(':')
        .next()
        .and_then(|p| p.trim_end_matches('/').parse().ok())
        .expect("decision_world wants an http://host:port container");
    let podman = Arc::new(RecordingPodman::default());
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    )));
    let mut s = Settings::default();
    s.vram.load_timeout_seconds = 2;
    // The start sequence refuses a class with no models dir before it renders
    // any argv; nothing is read from it here.
    s.aux_router.models_dir = std::env::temp_dir().display().to_string();
    store::save_settings(&state.db, &s).await.unwrap();
    for (model_id, kind) in [
        (EMBED, AuxKind::Embed),
        (RERANK, AuxKind::Rerank),
        (DECISION, AuxKind::Decision),
    ] {
        store::insert_aux_model(
            &state.db,
            &NewAuxModel {
                model_id: model_id.into(),
                gguf_path: format!("{model_id}.gguf"),
                kind,
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
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let gw = serve(state.clone()).await;
    DecisionWorld { state, gw, podman }
}

/// Mount the readiness probe the start sequence polls before it forwards.
pub async fn healthy(container: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/health"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .mount(container)
        .await;
}

// ---------------------------------------------------------------------------
// The wire, as its three references write it (decision-models design §1.2)
// ---------------------------------------------------------------------------

/// Appendix A.3's ticket with `model` left as `__MODEL__`. Compact between
/// its members, as a relay writes them, and spaced inside them, as a client
/// might: a relay that changed anything but `model` shows in a byte compare.
/// The options are not in alphabetical order, and their order is the wire's.
pub const TICKET: &str = r#"{"model":"__MODEL__","state":"Hi, my invoice from last month charged me twice for the Pro plan. Can you refund one of the charges? This is the second time this has happened and I am pretty annoyed.","questions":{
  "department": {"type": "choice", "instructions": "Which team should handle this ticket?",
    "criteria": {"billing": "payments, invoices, refunds, plan charges",
                 "technical": "bugs, outages, errors in the product",
                 "sales": "new purchases, upgrades, quotes", "other": null}},
  "urgency": {"type": "score", "instructions": "How urgent is this ticket?",
    "criteria": ["not urgent: can wait a week", "normal: within two days",
                 "urgent: same day", "critical: immediately"]},
  "angry": {"type": "noul", "instructions": "The customer is upset."}}}"#;

/// [`TICKET`] for the model `model`.
pub fn ticket(model: &str) -> String {
    TICKET.replace("__MODEL__", model)
}

/// Appendix A.4: llama.cpp's answer (b11549, values shortened to four
/// places), spaced as the probe printed it.
pub const LLAMA_ANSWER: &str = r#"{"model": "laya",
 "answers": {
  "department": {"type": "choice", "choice": "billing",
    "probabilities": {"billing": 0.9671, "technical": 0.0097, "sales": 0.0094, "other": 0.0138},
    "confidence": 0.9561},
  "department_bare": {"type": "choice", "choice": "billing",
    "probabilities": {"billing": 0.9457, "technical": 0.0218, "sales": 0.0173, "other": 0.0151},
    "confidence": 0.9277},
  "urgency": {"type": "score", "score": 1.6808,
    "legend": {"0": "not urgent: can wait a week", "1": "normal: within two days",
               "2": "urgent: same day", "3": "critical: immediately"},
    "probabilities": {"0": 0.0749, "1": 0.2918, "2": 0.5109, "3": 0.1224},
    "confidence": 0.4360},
  "angry": {"type": "noul", "noul": 0.8319}},
 "usage": {"input_tokens": 299, "output_tokens": 0}}"#;

/// OpenRouter's documented answer (the TypeSafe SDK guide,
/// openrouter.ai/docs/guides/community/typesafe-sdk.md, read 2026-10-12): its
/// extensions `id`, `provider` and `usage.cost`, and non-zero output tokens.
pub const OPENROUTER_ANSWER: &str = r#"{
  "id": "gen-dec-1789738314-X5e5eKGQdvR9rblyX250",
  "model": "typesafe/jev-1.13-20260917",
  "provider": "TypeSafe",
  "answers": {
    "refund": { "type": "noul", "noul": 0.98 }
  },
  "usage": { "input_tokens": 275, "output_tokens": 20, "cost": 0.00003 }
}"#;

/// TypeSafe's documented answers, one per question type
/// (docs.typesafe.ai/api.md, read 2026-10-12).
pub const TYPESAFE_ANSWERS: [&str; 3] = [
    r#"{"model": "jev-1.13.0",
  "answers": {"is_urgent": {"type": "noul", "noul": 0.95}},
  "usage": { "input_tokens": 296, "output_tokens": 20 }}"#,
    r#"{"model": "jev-1.13.0",
  "answers": {"department": {"type": "choice", "choice": "billing",
    "probabilities": { "billing": 0.88, "technical": 0.12, "sales": 0.0 },
    "confidence": 0.81}},
  "usage": { "input_tokens": 318, "output_tokens": 34 }}"#,
    r#"{"model": "jev-1.13.0",
  "answers": {"frustration": {"type": "score", "score": 1.05,
    "legend": { "0": "Calm", "1": "Frustrated", "2": "Very angry" },
    "probabilities": { "0": 0.0, "1": 0.95, "2": 0.05 },
    "confidence": 0.92}},
  "usage": { "input_tokens": 304, "output_tokens": 18 }}"#,
];
