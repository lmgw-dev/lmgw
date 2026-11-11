//! Get it, server-side (find-a-model design WP5, §6.2, §10): `model_get`
//! against a mock hub and a Range-capable mock CDN serving the synthetic
//! GGUFs `plan_pin` writes.
//!
//! - The files `hf_add` would fetch are downloaded, the file is planned
//!   from disk (the plan the hub gave before, `source` aside), fitted to
//!   the card, and the row created with the fitting context and its note,
//!   never started (the fake Podman records no start).
//! - An embedder goes into the aux dir as an aux row whose ubatch and batch
//!   are its fitted context (D26, D29).
//! - A failed download fails the get with its sentence; a second get of a
//!   file joins the first; a row already serving the file is the answer; a
//!   public name taken in another class gets `-2` (D28).
//! - A restart mid-download and after the create: the get goes on, and the
//!   row is created once (D122).
//! - A full disk: the free-space warning in the answer (D125), and a
//!   download that meets ENOSPC failing the get with its sentence, the
//!   `.part` kept.
//! - "Fit again" (D26): the context the card fits now, saved after a
//!   confirm, with the note renewed; any other change of the context drops
//!   the note.
//!
//! - [`review`]: WP5's review round (shared downloads, cancels, a waiting
//!   get's untracked or cancelled paused row, concurrent gets, a dropped
//!   request, the status read, pass-through names, Fit again's confirm and
//!   its aux row, the aux re-plan's rounds).
//!
//! The names are fake and nothing here reaches the network. Every test sets
//! `HF_ENDPOINT`, so each holds `common::process_env_lock` for its body.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_api_types::model_find::{FitMark, FitMeasure, FitNote};
use lmgw_api_types::model_get::{FitChange, ModelFitAgain, ModelGetDone, ModelGetStarted};
use lmgw_core::config::Settings;
use lmgw_core::erased::Erased;
use lmgw_core::jobs::{hf_download, model_get};
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::common::{self, patience, Gw};
use crate::plan_pin::{chat_header, drafter, embedder, projector};

mod review;

const REPO: &str = "demo-org/demo-chat-GGUF";
const EMBED_REPO: &str = "demo-org/demo-embedder-GGUF";
const COMMIT: &str = "2222222222222222222222222222222222222222";
const MIB: u64 = 1 << 20;
const WEIGHTS: &str = "demo-chat-Q4_K_M.gguf";
const PROJ_F16: &str = "mmproj-demo-F16.gguf";
const PROJ_F32: &str = "mmproj-demo-F32.gguf";
const DRAFTER: &str = "demo-chat-dflash-drafter.gguf";
const EMBED: &str = "demo-embedder-Q8_0.gguf";
const EMBED_PROJ: &str = "mmproj-demo-embedder-F16.gguf";

fn sha(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// A `podman` that agrees to everything, starts nothing, and records every
/// call.
#[derive(Default)]
struct Podman(Mutex<Vec<Vec<String>>>);

impl CommandRunner for Podman {
    fn run<'a>(
        &'a self,
        _program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        self.0.lock().unwrap().push(args.to_vec());
        Box::pin(async move {
            Ok(CmdOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        })
    }
}

impl Podman {
    /// The calls that would have started a model's container. The save's
    /// own check of the engine's flags runs `llama-server --help` in a
    /// throwaway container (`lmgw-help-…`), which loads no model.
    fn starts(&self) -> Vec<Vec<String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|a| {
                matches!(
                    a.first().map(String::as_str),
                    Some("run" | "start" | "create")
                )
            })
            .filter(|a| a.last().map(String::as_str) != Some("--help"))
            .cloned()
            .collect()
    }
}

/// What the mock CDN serves, by repo path: a `Range: bytes=N-` gets a `206`
/// from byte N, anything else the whole file. A path in `missing` answers
/// `404`; `delay` holds every answer back, `slow` those of its files longer.
#[derive(Clone, Default)]
struct Cdn {
    files: Arc<HashMap<String, Vec<u8>>>,
    missing: Arc<Vec<String>>,
    delay: Option<Duration>,
    slow: Arc<Vec<(String, Duration)>>,
    ranges: Arc<Mutex<Vec<Asked>>>,
}

/// One request the CDN saw: the file, and the Range it asked with.
type Asked = (String, Option<String>);

impl Cdn {
    fn answer(&self, r: &Request) -> ResponseTemplate {
        let file = r
            .url
            .path()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let range = r
            .headers
            .get("range")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.ranges
            .lock()
            .unwrap()
            .push((file.clone(), range.clone()));
        let answer = match (self.files.get(&file), self.missing.contains(&file)) {
            (Some(bytes), false) => {
                let total = bytes.len();
                let from = range
                    .as_deref()
                    .and_then(|v| v.strip_prefix("bytes="))
                    .and_then(|v| v.strip_suffix('-'))
                    .and_then(|v| v.parse::<usize>().ok());
                match from {
                    Some(from) if from < total => ResponseTemplate::new(206)
                        .insert_header("accept-ranges", "bytes")
                        .insert_header(
                            "content-range",
                            format!("bytes {from}-{}/{total}", total - 1).as_str(),
                        )
                        .set_body_bytes(bytes[from..].to_vec()),
                    _ => ResponseTemplate::new(200).set_body_bytes(bytes.clone()),
                }
            }
            _ => ResponseTemplate::new(404),
        };
        let slow = self.slow.iter().find(|(f, _)| *f == file).map(|(_, d)| *d);
        match slow.or(self.delay) {
            Some(d) => answer.set_delay(d),
            None => answer,
        }
    }

    fn ranges_of(&self, file: &str) -> Vec<Option<String>> {
        self.ranges
            .lock()
            .unwrap()
            .iter()
            .filter(|(f, _)| f == file)
            .map(|(_, r)| r.clone())
            .collect()
    }
}

/// A repo on the mock hub: its files, and a size the listing names instead
/// of the real one, per file.
struct Repo<'a> {
    name: &'a str,
    files: Vec<(&'a str, Vec<u8>)>,
    listed: Vec<(&'a str, u64)>,
}

/// The mock hub with `repos` and its CDN. The hub's answers are shaped as
/// WP1's tests record them.
async fn hub(repos: &[Repo<'_>], cdn: Cdn) -> (MockServer, MockServer, Cdn) {
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    let mut served: HashMap<String, Vec<u8>> = HashMap::new();
    for repo in repos {
        let listing: Vec<Value> = repo
            .files
            .iter()
            .map(|(p, b)| {
                let size = repo
                    .listed
                    .iter()
                    .find(|(f, _)| f == p)
                    .map(|(_, s)| *s)
                    .unwrap_or(b.len() as u64);
                json!({"type": "file", "path": p, "size": size,
                       "lfs": {"oid": sha(b), "size": size}})
            })
            .collect();
        Mock::given(method("GET"))
            .and(path(format!("/api/models/{}/tree/main", repo.name)))
            .respond_with(ResponseTemplate::new(200).set_body_json(listing))
            .mount(&hub)
            .await;
        for (p, b) in &repo.files {
            served.insert(p.to_string(), b.clone());
            Mock::given(method("GET"))
                .and(path(format!("/{}/resolve/main/{p}", repo.name)))
                .respond_with(
                    ResponseTemplate::new(302)
                        .insert_header("x-repo-commit", COMMIT)
                        .insert_header("x-linked-etag", format!("\"{}\"", sha(b)).as_str())
                        .insert_header("x-linked-size", b.len().to_string().as_str())
                        .insert_header(
                            "location",
                            format!("{}/cdn/{p}", cdn_server.uri()).as_str(),
                        ),
                )
                .mount(&hub)
                .await;
        }
    }
    let cdn = Cdn {
        files: Arc::new(served),
        ..cdn
    };
    let c = cdn.clone();
    Mock::given(method("GET"))
        .respond_with(move |r: &Request| c.answer(r))
        .mount(&cdn_server)
        .await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    (hub, cdn_server, cdn)
}

/// The chat repo: weights, two projectors (the rule takes F16), a drafter.
fn chat_repo() -> Repo<'static> {
    Repo {
        name: REPO,
        files: vec![
            (WEIGHTS, chat_header(131_072, 4, 2 * MIB).bytes()),
            (PROJ_F16, projector(MIB).bytes()),
            (PROJ_F32, projector(2 * MIB).bytes()),
            (DRAFTER, drafter().bytes()),
        ],
        listed: Vec::new(),
    }
}

fn embed_repo() -> Repo<'static> {
    Repo {
        name: EMBED_REPO,
        files: vec![
            (EMBED, embedder().bytes()),
            (EMBED_PROJ, projector(MIB).bytes()),
        ],
        listed: Vec::new(),
    }
}

struct World {
    state: SharedState,
    gw: Gw,
    podman: Arc<Podman>,
    dir: tempfile::TempDir,
}

/// Empty class dirs, a declared budget of `budget_mib` with 32 MiB of
/// headroom and no GPU telemetry, no engine image to ask Podman about, and
/// a Podman that starts nothing.
async fn world_on(db: Option<sqlx::SqlitePool>, dir: tempfile::TempDir, budget_mib: u64) -> World {
    let state = match db {
        Some(db) => AppState::init_for_tests_on(db).await.unwrap(),
        None => AppState::init_for_tests().await.unwrap(),
    };
    let podman = Arc::new(Podman::default());
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(|| Ok(1)),
    )));
    let mut settings = Settings::default();
    settings.router.models_dir = dir.path().join("chat").display().to_string();
    settings.aux_router.models_dir = dir.path().join("aux").display().to_string();
    // No image to fetch: nothing asks Podman whether one is here.
    settings.router.image = String::new();
    settings.aux_router.image = String::new();
    settings.vram.budget_mb = budget_mib;
    settings.vram.headroom_mb = 32;
    store::save_settings(&state.db, &settings).await.unwrap();
    state
        .vram
        .set_probe(Arc::new(lmgw_core::vram::nvml::NoTelemetry("none".into())));
    state.reload_snapshot().await.unwrap();
    let gw = common::serve(state.clone()).await;
    World {
        state,
        gw,
        podman,
        dir,
    }
}

async fn world(budget_mib: u64) -> World {
    world_on(None, tempfile::tempdir().unwrap(), budget_mib).await
}

async fn op(gw: &Gw, name: &str, args: Value) -> (u16, Value) {
    let resp = gw
        .client()
        .post(format!("{gw}/api/op/{name}"))
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{name}: {e}: {body}")),
    )
}

async fn get(gw: &Gw, args: Value) -> ModelGetStarted {
    let (status, live) = op(gw, "model_get", args.clone()).await;
    assert_eq!(status, 200, "model_get {args}: {live}");
    common::round_trips::<ModelGetStarted>("POST /api/op/model_get", &live)
}

/// The get `id` once it has ended, and its job's result or error.
async fn ended(
    state: &SharedState,
    id: i64,
) -> (store::ModelGetRow, Option<ModelGetDone>, Option<String>) {
    let db = state.db.clone();
    patience::until_async(&format!("get {id} to end"), || {
        let db = db.clone();
        async move {
            store::get_model_get(&db, id)
                .await
                .unwrap()
                .is_some_and(|g| g.status != "running")
        }
    })
    .await;
    let row = store::get_model_get(&state.db, id).await.unwrap().unwrap();
    let key = model_get::job_key(&row.repo, &row.file);
    // The job row ends a moment after the get's own row.
    patience::until_async("the get's job to end", || {
        let (db, key) = (state.db.clone(), key.clone());
        async move {
            store::latest_job_by_key(&db, model_get::JOB_KIND, &key)
                .await
                .unwrap()
                .is_some_and(|j| !matches!(j.status.as_str(), "queued" | "running"))
        }
    })
    .await;
    let job = store::latest_job_by_key(&state.db, model_get::JOB_KIND, &key)
        .await
        .unwrap()
        .unwrap();
    let done = job
        .result
        .as_deref()
        .and_then(|r| serde_json::from_str::<ModelGetDone>(r).ok());
    (row, done, job.error)
}

fn note_of(text: Option<String>) -> Option<FitNote> {
    text.map(|t| serde_json::from_str(&t).unwrap())
}

/// The whole get of a chat model: its files (one projector of two, the
/// drafter), the plan from disk the hub's, the row at the fitting context
/// with its note, and nothing started.
#[tokio::test]
async fn a_get_downloads_plans_fits_and_creates_the_row_without_starting_it() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, cdn) = hub(&[chat_repo()], Cdn::default()).await;

    let from_hub = lmgw_core::modelinfo::local_model_plan_of(
        &w.state,
        None,
        Some(REPO),
        Some(WEIGHTS),
        Some("chat"),
    )
    .await
    .unwrap();
    let started = get(&w.gw, json!({"repo": REPO, "file": WEIGHTS})).await;
    assert_eq!(started.model_id, "demo-chat");
    assert_eq!(started.public_name, "demo-chat");
    assert_eq!(started.target, "chat");
    assert_eq!(started.measure, FitMeasure::Beside);
    assert!(!started.joined);
    let mut files: Vec<&str> = started.downloads.iter().map(|d| d.file.as_str()).collect();
    files.sort();
    assert_eq!(files, [WEIGHTS, DRAFTER, PROJ_F16]);
    let companions: Vec<&str> = started.companions.iter().map(|c| c.file.as_str()).collect();
    assert!(companions.contains(&PROJ_F16) && !companions.contains(&PROJ_F32));
    assert!(started.disk.free_bytes.is_some());
    assert!(!started.disk.short, "{:?}", started.disk);
    assert!(started.disk.needs_bytes > 12 * MIB, "{:?}", started.disk);

    let (row, done, error) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    let done = done.expect("a result");
    assert!(done.created);
    assert_eq!(done.model_id, "demo-chat");
    let fit = done.fit.clone().unwrap();
    assert_eq!(fit.mark, FitMark::FitsShorter, "{fit:?}");
    assert_eq!(fit.trained_ctx, Some(131_072));
    let ctx = fit.ctx.unwrap();
    assert!(ctx < 131_072);
    assert_eq!(done.ctx_size, Some(ctx));

    // The row, at the fitting context, with the note.
    let snap = w.state.snapshot();
    let rows: Vec<_> = snap
        .local_models
        .iter()
        .filter(|m| m.model_id == "demo-chat")
        .collect();
    assert_eq!(rows.len(), 1);
    let created = rows[0];
    assert_eq!(created.id, done.row_id);
    assert_eq!(created.params.ctx_size, Some(ctx as i64));
    assert_eq!(created.gguf_path, format!("{REPO}/{WEIGHTS}"));
    assert_eq!(
        created.params.mmproj_path.as_deref(),
        Some(format!("{REPO}/{PROJ_F16}").as_str())
    );
    let note = note_of(
        store::fit_note(&w.state.db, "chat", created.id)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(note.ctx, ctx);
    assert_eq!(note.trained_ctx, Some(131_072));
    assert_eq!(note.measure, FitMeasure::Beside);
    assert_eq!(Some(&note), done.note.as_ref());

    // The plan the job read from disk is the hub's: same parameters.
    let from_disk = lmgw_core::modelinfo::local_model_plan(
        &w.state,
        &format!("{REPO}/{WEIGHTS}"),
        Some("chat"),
    )
    .await
    .unwrap();
    assert_eq!(from_disk["params"], from_hub["params"]);
    assert_eq!(from_disk["source"]["from"], "disk");

    // Never started: no container, no start of any kind.
    assert!(w.podman.starts().is_empty(), "{:?}", w.podman.starts());
    // The hub plan's header read, then the download once, whole; the wider
    // projector never.
    assert_eq!(
        cdn.ranges_of(WEIGHTS),
        vec![Some("bytes=0-".to_string()), None]
    );
    assert!(cdn.ranges_of(PROJ_F32).is_empty());
    assert!(w.dir.path().join("chat").join(REPO).join(WEIGHTS).is_file());
}

/// An embedder, its class read from the header: the aux dir, an aux row
/// whose ubatch and batch are its fitted context, its projector and no
/// drafter, and "Ready for Knowledge".
#[tokio::test]
async fn an_embedder_becomes_an_aux_row_with_its_batch_tied_to_the_fitted_context() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, _cdn) = hub(&[embed_repo()], Cdn::default()).await;

    let started = get(&w.gw, json!({"repo": EMBED_REPO, "file": EMBED})).await;
    assert_eq!(started.target, "aux");
    assert_eq!(started.model_id, "demo-embedder");
    assert_eq!(started.public_name, "embed/demo-embedder");
    let mut files: Vec<&str> = started.downloads.iter().map(|d| d.file.as_str()).collect();
    files.sort();
    assert_eq!(files, [EMBED, EMBED_PROJ]);

    let (row, done, error) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    let done = done.unwrap();
    assert!(
        done.next_step.contains("Ready for Knowledge"),
        "{}",
        done.next_step
    );
    assert!(w
        .dir
        .path()
        .join("aux")
        .join(EMBED_REPO)
        .join(EMBED)
        .is_file());
    let snap = w.state.snapshot();
    let aux = snap
        .aux_models
        .iter()
        .find(|m| m.model_id == "demo-embedder")
        .unwrap();
    let ctx = aux.ctx_size.unwrap();
    assert!(ctx < 32_768, "fitted below the trained context: {ctx}");
    assert_eq!(
        aux.ubatch_size,
        Some(ctx),
        "the ubatch moves with the context"
    );
    assert_eq!(aux.batch_size, Some(ctx), "and the batch");
    let note = note_of(store::fit_note(&w.state.db, "aux", aux.id).await.unwrap()).unwrap();
    assert_eq!(note.ctx, ctx as u64);
    assert_eq!(note.trained_ctx, Some(32_768));
    assert!(w.podman.starts().is_empty(), "{:?}", w.podman.starts());
}

/// A download that fails fails the get with its sentence, and creates
/// nothing.
#[tokio::test]
async fn a_failed_download_fails_the_get_with_its_sentence() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let cdn = Cdn {
        missing: Arc::new(vec![WEIGHTS.to_string()]),
        ..Cdn::default()
    };
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], cdn).await;
    let started = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    let (row, done, error) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "failed");
    assert!(done.is_none());
    let said = row.error.unwrap();
    assert!(
        said.starts_with(&format!("{WEIGHTS} didn't download")),
        "{said}"
    );
    assert!(said.contains("404"), "{said}");
    assert_eq!(error.as_deref(), Some(said.as_str()));
    assert!(w.state.snapshot().local_models.is_empty());
}

/// A second get of the file while the first runs is the first.
#[tokio::test]
async fn a_second_get_of_the_same_file_joins_the_first() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let cdn = Cdn {
        delay: Some(Duration::from_millis(800)),
        ..Cdn::default()
    };
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], cdn).await;
    let first = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    let second = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    assert!(second.joined);
    assert_eq!(second.get_id, first.get_id);
    assert_eq!(second.job_id, first.job_id);
    assert_eq!(second.model_id, first.model_id);
    let (row, _, error) = ended(&w.state, first.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    assert_eq!(w.state.snapshot().local_models.len(), 1);
}

/// The file on disk and a row serving it: the op answers that row up
/// front (D87), its own id and name, with nothing queued, no get and no
/// job.
#[tokio::test]
async fn a_row_already_serving_the_file_is_the_answer() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, cdn) = hub(&[chat_repo()], Cdn::default()).await;
    // Downloaded before, with its companions, and set up.
    let first = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    let (_, done, _) = ended(&w.state, first.get_id.unwrap()).await;
    let row_id = done.unwrap().row_id;
    let asked = cdn.ranges.lock().unwrap().len();

    let again = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat", "model_id": "other-name"}),
    )
    .await;
    assert!(!again.joined);
    assert_eq!(again.row_id, Some(row_id));
    assert_eq!(
        again.model_id, "demo-chat",
        "the row's own id, not a new one"
    );
    assert_eq!(again.public_name, "demo-chat");
    assert_eq!((again.get_id, again.job_id), (None, None));
    assert!(again.downloads.is_empty(), "{:?}", again.downloads);
    assert!(
        again.message.contains("already serves"),
        "{}",
        again.message
    );
    assert_eq!(
        cdn.ranges.lock().unwrap().len(),
        asked,
        "nothing downloaded again"
    );
    assert_eq!(w.state.snapshot().local_models.len(), 1);
    assert!(store::running_model_gets(&w.state.db)
        .await
        .unwrap()
        .is_empty());
}

/// A public name taken in another class: the aux class without a prefix
/// serves an embedder as `demo-chat`, so the chat row is `demo-chat-2`
/// (D28).
#[tokio::test]
async fn a_public_name_taken_in_another_class_gets_a_suffix() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let mut settings = w.state.snapshot().settings.clone();
    settings.aux_router.public_prefix = String::new();
    store::save_settings(&w.state.db, &settings).await.unwrap();
    embedder().write_to(&w.dir.path().join("aux/x/e.gguf"));
    store::insert_aux_model(
        &w.state.db,
        &store::NewAuxModel {
            model_id: "demo-chat".into(),
            gguf_path: "x/e.gguf".into(),
            kind: lmgw_core::config::AuxKind::Embed,
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
            fallback: None,
        },
    )
    .await
    .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;
    let started = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    assert_eq!(started.model_id, "demo-chat-2");
    let (row, done, error) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "done", "{error:?}");
    assert_eq!(done.unwrap().public_name, "demo-chat-2");
}

/// The process stopped mid-download with a get waiting on it; a new one on
/// the same database resumes the download from its `.part`, and the get
/// goes on and creates the row. A get cut after it created the row (its
/// own row still `running`) answers that row at the next boot: one row.
#[tokio::test]
async fn a_restart_mid_get_goes_on_and_creates_the_row_once() {
    let _env = common::process_env_lock().await;
    let dir = tempfile::tempdir().unwrap();
    let before = world_on(None, dir, 160).await;
    let repo = Repo {
        name: REPO,
        files: vec![(WEIGHTS, chat_header(131_072, 4, 2 * MIB).bytes())],
        listed: Vec::new(),
    };
    let bytes = repo.files[0].1.clone();
    let (_hub, _cdn_server, cdn) = hub(&[repo], Cdn::default()).await;

    // As a shutdown leaves it: the download `downloading` with a `.part`
    // and its hash, its job `running`; the get `running`, its job too.
    let dest = format!("{REPO}/{WEIGHTS}");
    let id = store::upsert_hf_model(&before.state.db, REPO, WEIGHTS, &dest, "chat")
        .await
        .unwrap();
    store::set_hf_status(&before.state.db, id, "downloading", None)
        .await
        .unwrap();
    store::set_hf_oid(&before.state.db, id, Some(&sha(&bytes)))
        .await
        .unwrap();
    let part = before.dir.path().join("chat").join(format!("{dest}.part"));
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    let cut = bytes.len() / 3;
    std::fs::write(&part, &bytes[..cut]).unwrap();
    for (kind, key, input) in [
        (
            "hf_download",
            hf_download::job_key(id),
            json!({"hf_model_id": id}),
        ),
        (
            "model_get",
            model_get::job_key(REPO, WEIGHTS),
            json!({"get_id": 1}),
        ),
    ] {
        let job = store::claim_job(
            &before.state.db,
            kind,
            Some(&key),
            "cut",
            &input.to_string(),
        )
        .await
        .unwrap()
        .unwrap();
        store::start_job(&before.state.db, job).await.unwrap();
    }
    let get_id = store::insert_model_get(
        &before.state.db,
        &store::NewModelGet {
            repo: REPO,
            file: WEIGHTS,
            target: "chat",
            measure: "beside",
            model_id: "demo-chat",
            kind: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(get_id, 1);
    store::set_model_get_downloads(&before.state.db, get_id, &[id], &[id])
        .await
        .unwrap();

    // A new process on the same database, in the boot's order.
    let World { dir, state, .. } = before;
    let after = world_on(Some(state.db.clone()), dir, 160).await;
    assert_eq!(store::fail_orphaned_jobs(&after.state.db).await.unwrap(), 2);
    hf_download::resume_at_boot(&after.state).await;
    assert_eq!(
        model_get::reattach_at_boot(&after.state).await,
        vec![get_id]
    );
    let (row, done, error) = ended(&after.state, get_id).await;
    assert_eq!(row.status, "done", "{error:?}");
    assert!(done.unwrap().created);
    assert_eq!(cdn.ranges_of(WEIGHTS), vec![Some(format!("bytes={cut}-"))]);
    assert_eq!(
        std::fs::read(after.dir.path().join("chat").join(&dest)).unwrap(),
        bytes
    );
    assert_eq!(after.state.snapshot().local_models.len(), 1);

    // Cut again after the create, before the get recorded the row or wrote
    // its note: the next boot answers the row it made, and writes the note
    // the get recorded for it.
    let row_id = after.state.snapshot().local_models[0].id;
    let note = store::fit_note(&after.state.db, "chat", row_id)
        .await
        .unwrap()
        .expect("the fit's note");
    sqlx::query(
        "UPDATE model_gets SET status = 'running', finished_at = NULL, row_id = NULL \
         WHERE id = ?1",
    )
    .bind(get_id)
    .execute(&after.state.db)
    .await
    .unwrap();
    store::set_fit_note(&after.state.db, "chat", row_id, None)
        .await
        .unwrap();
    let World { dir, state, .. } = after;
    let third = world_on(Some(state.db.clone()), dir, 160).await;
    store::fail_orphaned_jobs(&third.state.db).await.unwrap();
    assert_eq!(
        model_get::reattach_at_boot(&third.state).await,
        vec![get_id]
    );
    let (row, done, error) = ended(&third.state, get_id).await;
    assert_eq!(row.status, "done", "{error:?}");
    let done = done.unwrap();
    assert!(!done.created, "answered, not created again");
    assert_eq!(done.model_id, "demo-chat");
    assert_eq!(done.row_id, row_id);
    assert_eq!(third.state.snapshot().local_models.len(), 1);
    assert_eq!(
        store::fit_note(&third.state.db, "chat", row_id)
            .await
            .unwrap()
            .as_deref(),
        Some(note.as_str()),
        "the note the cut get didn't write"
    );
    assert!(third.podman.starts().is_empty());
}

/// More than the disk holds: the answer says so and the get goes ahead (a
/// warning, never a block).
#[tokio::test]
async fn a_get_larger_than_the_free_disk_warns_in_its_answer() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let mut repo = chat_repo();
    // The listing names a file no disk holds.
    repo.listed = vec![(WEIGHTS, 1 << 60)];
    let cdn = Cdn {
        missing: Arc::new(vec![WEIGHTS.to_string()]),
        ..Cdn::default()
    };
    let (_hub, _cdn_server, _cdn) = hub(&[repo], cdn).await;
    let started = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    assert!(started.disk.short, "{:?}", started.disk);
    let said = started.disk.sentence.clone().unwrap();
    assert!(
        said.starts_with("Needs ") && said.contains("on the disk") && said.ends_with("is free"),
        "{said}"
    );
    assert!(started.disk.needs_bytes >= 1 << 60);
    assert!(started.job_id.is_some(), "queued all the same");
    ended(&w.state, started.get_id.unwrap()).await;
}

/// A download that meets a full disk (ENOSPC, from `/dev/full` behind the
/// `.part`) fails the get with the full disk's sentence, and the `.part`
/// stays for Retry.
#[cfg(unix)]
#[tokio::test]
async fn a_full_disk_fails_the_get_with_its_sentence_and_keeps_the_part() {
    if !std::path::Path::new("/dev/full").exists() {
        eprintln!("no /dev/full here: skipped");
        return;
    }
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let repo = Repo {
        name: REPO,
        files: vec![(WEIGHTS, chat_header(131_072, 4, 2 * MIB).bytes())],
        listed: Vec::new(),
    };
    let (_hub, _cdn_server, _cdn) = hub(&[repo], Cdn::default()).await;
    let part = w
        .dir
        .path()
        .join("chat")
        .join(REPO)
        .join(format!("{WEIGHTS}.part"));
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/dev/full", &part).unwrap();

    let started = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    let (row, _, _) = ended(&w.state, started.get_id.unwrap()).await;
    assert_eq!(row.status, "failed");
    let said = row.error.unwrap();
    assert!(said.contains("The disk is full"), "{said}");
    assert!(
        said.contains("is free") && said.contains("Get it again goes on"),
        "{said}"
    );
    assert!(std::fs::symlink_metadata(&part).is_ok(), "the .part stays");
    assert!(w.state.snapshot().local_models.is_empty());
}

/// "Fit again": with more room the context grows back, saved only after a
/// confirm of the changes as shown, the note renewed (none once the whole
/// context fits); a stale confirm writes nothing; a save of another context
/// drops the note.
#[tokio::test]
async fn fit_again_saves_what_the_card_fits_now_after_a_confirm() {
    let _env = common::process_env_lock().await;
    let w = world(160).await;
    let (_hub, _cdn_server, _cdn) = hub(&[chat_repo()], Cdn::default()).await;
    let started = get(
        &w.gw,
        json!({"repo": REPO, "file": WEIGHTS, "target": "chat"}),
    )
    .await;
    let (_, done, error) = ended(&w.state, started.get_id.unwrap()).await;
    let done = done.unwrap_or_else(|| panic!("{error:?}"));
    let fitted = done.ctx_size.unwrap();

    // The same card: nothing to change.
    let ask = json!({"target": "chat", "model_id": "demo-chat"});
    let (status, live) = op(&w.gw, "model_fit_again", ask.clone()).await;
    assert_eq!(status, 200, "{live}");
    let same = common::round_trips::<ModelFitAgain>("POST /api/op/model_fit_again", &live);
    assert!(same.changes.is_empty(), "{same:?}");
    assert_eq!(same.fit.ctx, Some(fitted));
    assert_eq!(same.note.as_ref().map(|n| n.ctx), Some(fitted));

    // A larger budget.
    let mut settings = w.state.snapshot().settings.clone();
    settings.vram.budget_mb = 4096;
    store::save_settings(&w.state.db, &settings).await.unwrap();
    w.state.reload_snapshot().await.unwrap();
    let (_, live) = op(&w.gw, "model_fit_again", ask.clone()).await;
    let offer: ModelFitAgain = serde_json::from_value(live).unwrap();
    assert!(!offer.saved);
    assert_eq!(offer.fit.mark, FitMark::Fits, "{offer:?}");
    assert_eq!(
        offer.changes,
        vec![FitChange {
            key: "ctx_size".into(),
            from: Some(fitted),
            to: Some(131_072)
        }]
    );
    let row = w.state.snapshot().local_models[0].clone();
    assert_eq!(
        row.params.ctx_size,
        Some(fitted as i64),
        "nothing written yet"
    );

    // A confirm made from another row than this one writes nothing.
    let stale = vec![FitChange {
        key: "ctx_size".into(),
        from: Some(fitted + 1),
        to: Some(65_536),
    }];
    let (_, live) = op(
        &w.gw,
        "model_fit_again",
        json!({"target": "chat", "model_id": "demo-chat", "confirm": stale}),
    )
    .await;
    let outdated: ModelFitAgain = serde_json::from_value(live).unwrap();
    assert!(outdated.outdated && !outdated.saved);
    assert_eq!(
        w.state.snapshot().local_models[0].params.ctx_size,
        Some(fitted as i64)
    );

    // The confirm of the changes as shown saves them; the whole context
    // fits, so the note goes.
    let (_, live) = op(
        &w.gw,
        "model_fit_again",
        json!({"target": "chat", "model_id": "demo-chat", "confirm": offer.changes}),
    )
    .await;
    let saved: ModelFitAgain = serde_json::from_value(live).unwrap();
    assert!(saved.saved, "{saved:?}");
    assert!(saved.note.is_none());
    let row = w.state.snapshot().local_models[0].clone();
    assert_eq!(row.params.ctx_size, Some(131_072));
    assert!(store::fit_note(&w.state.db, "chat", row.id)
        .await
        .unwrap()
        .is_none());

    // Back to the small card: Fit again shortens it and writes the note.
    let mut settings = w.state.snapshot().settings.clone();
    settings.vram.budget_mb = 160;
    store::save_settings(&w.state.db, &settings).await.unwrap();
    w.state.reload_snapshot().await.unwrap();
    let (_, live) = op(&w.gw, "model_fit_again", ask.clone()).await;
    let offer: ModelFitAgain = serde_json::from_value(live).unwrap();
    assert_eq!(offer.changes[0].to, Some(fitted));
    let (_, live) = op(
        &w.gw,
        "model_fit_again",
        json!({"target": "chat", "model_id": "demo-chat", "confirm": offer.changes}),
    )
    .await;
    let saved: ModelFitAgain = serde_json::from_value(live).unwrap();
    assert!(saved.saved);
    assert_eq!(saved.note.as_ref().map(|n| n.ctx), Some(fitted));
    assert!(store::fit_note(&w.state.db, "chat", row.id)
        .await
        .unwrap()
        .is_some());

    // The editor saving another context: the note no longer stands.
    let (status, body) = op(
        &w.gw,
        "local_model_set",
        json!({"action": "update", "id": row.id, "ctx_size": 4096}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(store::fit_note(&w.state.db, "chat", row.id)
        .await
        .unwrap()
        .is_none());
    assert!(w.podman.starts().is_empty(), "{:?}", w.podman.starts());
}

/// The id a get derives (D28).
#[test]
fn the_id_is_the_repos_name_in_lower_case_without_its_gguf_suffix() {
    use lmgw_core::ops::id_of_repo;
    assert_eq!(id_of_repo("Qwen/Qwen3.6-27B"), "qwen3.6-27b");
    assert_eq!(id_of_repo("demo-org/Demo-Model-GGUF"), "demo-model");
    assert_eq!(id_of_repo("demo-org/demo_model_gguf"), "demo_model");
    assert_eq!(id_of_repo("demo-org/GGUF"), "gguf");
}
