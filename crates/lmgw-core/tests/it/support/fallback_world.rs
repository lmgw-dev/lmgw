//! A gateway with one model or alias of every kind and type, for the
//! fallback save checks and the renames (openai egress design §14.2): the
//! types come from the rows themselves and, for the cloud aliases, from
//! their capability overrides — the catalog behind them is a closed port,
//! so an alias without an override is one whose type lmgw cannot know.
//!
//! - local chat rows `qwen3` (vision, tools) and `other`;
//! - aux rows `bge` (an embedder, `embed/bge`) and `bge-rr` (a reranker);
//! - audio rows `whisper` (asr) and `kokoro` (tts), `audio/…`;
//! - image row `flux`, `image/flux`;
//! - plain aliases on upstream `cloud`: `cloud-chat` (vision), `cloud-mini`
//!   (text only), `cloud-embed`, `cloud-asr`, `cloud-tts`, `bare` (no
//!   override: type unknown), and `cloud-b` on the same upstream;
//! - an `expose_all` upstream `kilo` (prefix `kilo`): passthrough ids
//!   `kilo/<model>`;
//! - candidate alias `smart` (`qwen3`).
//!
//! The container runtime answers no command: nothing starts.

use std::sync::Arc;

use lmgw_core::erased::Erased;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};

/// Answers every command with nothing: no save in these tests runs one that
/// matters.
struct Silent;

impl CommandRunner for Silent {
    fn run<'a>(
        &'a self,
        _program: &'a str,
        _args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        Box::pin(async {
            Ok(CmdOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        })
    }
}

pub struct World {
    pub state: SharedState,
    pub cloud: i64,
    _dir: tempfile::TempDir,
}

/// A capability override stating `task` and its input modalities (with
/// `vision` when they take images).
fn says(task: &str, inputs: &[&str]) -> Option<Value> {
    let vision = inputs.contains(&"image");
    Some(json!({"capabilities": {"task": task, "input_modalities": inputs, "vision": vision}}))
}

pub async fn upstream(state: &SharedState, name: &str, expose: bool) -> i64 {
    store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: name.into(),
            protocol: lmgw_core::config::Protocol::OpenaiCompatible,
            kind: lmgw_core::config::UpstreamKind::Generic,
            // A closed port: a catalog probe is refused at once.
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 1_000,
            enabled: true,
            expose_all: expose,
            expose_prefix: if expose { name.into() } else { String::new() },
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap()
}

pub async fn alias(state: &SharedState, name: &str, upstream: i64, over: Option<Value>) {
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: name.into(),
            upstream_id: upstream,
            upstream_model_id: format!("{name}-model"),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: over,
            fallback: None,
        },
    )
    .await
    .unwrap();
}

pub async fn world() -> World {
    let state = AppState::init_for_tests().await.unwrap();
    state.set_runtime_for_tests(Arc::new(Registry::new(
        Arc::new(Silent),
        reqwest::Client::new(),
    )));
    let dir = tempfile::tempdir().unwrap();
    for f in ["chat.gguf", "embed.gguf", "diffusion.gguf"] {
        std::fs::write(dir.path().join(f), b"x").unwrap();
    }
    let mut s = state.snapshot().settings.clone();
    let d = dir.path().display().to_string();
    s.router.models_dir = d.clone();
    s.aux_router.models_dir = d.clone();
    s.audio.models_dir = d.clone();
    s.image.models_dir = d;
    store::save_settings(&state.db, &s).await.unwrap();

    let cloud = upstream(&state, "cloud", false).await;
    upstream(&state, "kilo", true).await;
    alias(
        &state,
        "cloud-chat",
        cloud,
        says("chat", &["text", "image"]),
    )
    .await;
    alias(&state, "cloud-mini", cloud, says("chat", &["text"])).await;
    alias(&state, "cloud-b", cloud, says("chat", &["text"])).await;
    alias(&state, "cloud-embed", cloud, says("embedding", &["text"])).await;
    alias(&state, "cloud-asr", cloud, says("asr", &["audio"])).await;
    alias(&state, "cloud-tts", cloud, says("tts", &["text"])).await;
    alias(&state, "bare", cloud, None).await;

    for (id, over) in [
        (
            "qwen3",
            json!({"capabilities": {"task": "chat", "input_modalities": ["text", "image"],
                                    "vision": true}}),
        ),
        (
            "other",
            json!({"capabilities": {"task": "chat", "input_modalities": ["text"], "vision": false}}),
        ),
    ] {
        sqlx::query(
            "INSERT INTO local_models (model_id, gguf_path, params, args, idle_seconds, enabled, \
             public, warm_start, capabilities_override) \
             VALUES (?1, 'chat.gguf', '{}', '[]', 300, 1, 1, 0, ?2)",
        )
        .bind(id)
        .bind(over.to_string())
        .execute(&state.db)
        .await
        .unwrap();
    }
    for (id, kind) in [("bge", "embed"), ("bge-rr", "rerank")] {
        sqlx::query(
            "INSERT INTO aux_models (model_id, gguf_path, kind) VALUES (?1, 'embed.gguf', ?2)",
        )
        .bind(id)
        .bind(kind)
        .execute(&state.db)
        .await
        .unwrap();
    }
    for (id, task) in [("whisper", "asr"), ("kokoro", "tts")] {
        sqlx::query(
            "INSERT INTO audio_models (model_id, family, path, task) VALUES (?1, 'f', ?1, ?2)",
        )
        .bind(id)
        .bind(task)
        .execute(&state.db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO image_models (model_id, files) \
         VALUES ('flux', '{\"diffusion_model\": \"diffusion.gguf\"}')",
    )
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO candidate_aliases (alias, candidates) VALUES ('smart', '[\"qwen3\"]')",
    )
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    World {
        state,
        cloud,
        _dir: dir,
    }
}

impl World {
    /// The row id of `name` in `table` (`model_id`, or `alias` for the alias
    /// tables).
    pub async fn id(&self, table: &str, name: &str) -> i64 {
        let key = match table {
            "models" | "candidate_aliases" => "alias",
            _ => "model_id",
        };
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT id FROM {table} WHERE {key} = ?1"
        )))
        .bind(name)
        .fetch_one(&self.state.db)
        .await
        .unwrap()
    }

    /// The stored fallback of `name` in `table`.
    pub async fn fallback(&self, table: &str, name: &str) -> Option<String> {
        let key = match table {
            "models" | "candidate_aliases" => "alias",
            _ => "model_id",
        };
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT fallback FROM {table} WHERE {key} = ?1"
        )))
        .bind(name)
        .fetch_one(&self.state.db)
        .await
        .unwrap()
    }
}
