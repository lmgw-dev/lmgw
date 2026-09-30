//! A small GPU world for the background-admission suite
//! (`background_admission.rs`): chat rows on sparse weight files, a fake
//! driver whose free memory follows what the fake `podman` has loaded, and
//! one killable wiremock container per start.
//!
//! The same shape as `vram_admission/fixture.rs` and its siblings, cut to what
//! background traffic needs — that fixture is not shared, and this one is not
//! meant to grow into it. Nothing here touches a GPU or podman: `run` marks a
//! model loaded at the size of the file its `-m` names, `stop` unloads it, and
//! the driver reports `total − loaded − outside` as free.
//!
//! The request gate's candidate walk (`candidate_routing.rs`) adds a little:
//! containers that refuse a prompt as llama-server does one above its context,
//! a `/tokenize` count for ladder sends, per-process attribution for §4.7's
//! outside-VRAM verdict, rows and candidate aliases stored as given, and cloud
//! aliases on a wiremock upstream. The compatibility counters
//! (`count_compat.rs`) add `/apply-template`'s refusal of media a container
//! cannot take ([`World::projectors`]).

#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::config::{HoldFallbackMode, LlamaParams, Protocol, Route, Settings, UpstreamKind};
use lmgw_core::ladder::Rung;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry, HOST_PID_FORMAT};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewCandidateAlias, NewLocalModel, NewUpstream};
use lmgw_core::vram::{GpuMemory, GpuProbe, ProcessMemory};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

pub const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Default)]
pub struct World {
    /// Weights file name -> bytes it takes once loaded.
    pub files: HashMap<String, u64>,
    /// Model id -> bytes its running container holds (the file it loaded).
    pub size: HashMap<String, u64>,
    pub loaded: HashSet<String>,
    /// Container name -> model id, from `--name` and the model label.
    pub names: HashMap<String, String>,
    /// Host port -> model id, from `-p`.
    pub ports: HashMap<u16, String>,
    /// Models whose `/slots` reports a generation in progress.
    pub busy: HashSet<String>,
    /// Model ids started / stopped, in order.
    pub runs: Vec<String>,
    pub stops: Vec<String>,
    /// Bytes held by something that is not lmgw's — a game.
    pub outside: u64,
    /// Models whose container refuses every chat request the way
    /// llama-server refuses a prompt above its context, before any work
    /// (`exceed_context_size_error`).
    pub refuse_context: HashSet<String>,
    /// How many tokens `/tokenize` counts for any prompt — what a ladder's
    /// count beside its send reads.
    pub prompt_tokens: u64,
    /// The bodies `/apply-template` and `/tokenize` were posted, in order —
    /// what the compatibility counters (`count_compat.rs`) sent.
    pub templated: Vec<serde_json::Value>,
    pub tokenized: Vec<serde_json::Value>,
    /// The model of every chat answer a container gave, in order.
    pub chats: Vec<String>,
    /// Models whose container was started with a projector (`--mmproj`).
    /// Their `/apply-template` renders an image part; every other container
    /// refuses one, and every container refuses audio — the fake projector
    /// sees but does not hear — as llama-server does, with a 500.
    pub projectors: HashSet<String>,
    /// Per-process attribution (candidate-aliases §4.7): the driver lists one
    /// process per loaded model — its container's PID, handed out at `run` and
    /// answered by `inspect … {{.State.Pid}}` — plus one for `outside`. Off,
    /// it lists none, and the outside-VRAM verdict is unavailable.
    pub attribution: bool,
    /// Model id -> the host PID of its current container.
    pub pids: HashMap<String, u32>,
    /// Models whose `podman run` fails, as a start that cannot come up does.
    /// The attempt is still recorded in [`Self::runs`].
    pub fail_run: HashSet<String>,
    /// When set, every `podman run` is recorded in [`Self::runs`] and then
    /// waits here until the value reads `true` — a start held in flight, for
    /// a test that joins it or stops it ([`Gpu::gate_runs`]).
    pub run_gate: Option<tokio::sync::watch::Receiver<bool>>,
    /// When set, every `podman stop` waits here until the value reads `true`
    /// before it unloads anything — an eviction held in flight, with the
    /// admission gate held by whoever evicts ([`Gpu::gate_stops`]).
    pub stop_gate: Option<tokio::sync::watch::Receiver<bool>>,
}

/// The fake host PID of the process outside lmgw.
const OUTSIDE_PID: u32 = 7_000_000;

impl World {
    fn used(&self) -> u64 {
        let own: u64 = self.loaded.iter().filter_map(|m| self.size.get(m)).sum();
        own + self.outside
    }
}

struct FakeGpu {
    total: u64,
    world: Arc<Mutex<World>>,
}

impl GpuProbe for FakeGpu {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        let used = self.world.lock().unwrap().used();
        Ok(vec![GpuMemory {
            index: 0,
            name: "FakeGPU".into(),
            total_bytes: self.total,
            used_bytes: used,
            free_bytes: self.total.saturating_sub(used),
        }])
    }

    fn source(&self) -> String {
        "FakeGPU".into()
    }

    fn process_support(&self) -> Result<(), String> {
        if self.world.lock().unwrap().attribution {
            Ok(())
        } else {
            Err("FakeGPU lists no processes".into())
        }
    }

    fn processes(&self, _own: &[u32], _retired: &[u32]) -> Result<Vec<ProcessMemory>, String> {
        let w = self.world.lock().unwrap();
        if !w.attribution {
            return Err("FakeGPU lists no processes".into());
        }
        let mut out: Vec<ProcessMemory> = w
            .loaded
            .iter()
            .filter_map(|m| {
                Some(ProcessMemory {
                    pid: *w.pids.get(m)?,
                    bytes: w.size.get(m).copied(),
                })
            })
            .collect();
        if w.outside > 0 {
            out.push(ProcessMemory {
                pid: OUTSIDE_PID,
                bytes: Some(w.outside),
            });
        }
        out.sort_by_key(|p| p.pid);
        Ok(out)
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

struct Podman {
    world: Arc<Mutex<World>>,
}

#[async_trait::async_trait]
impl CommandRunner for Podman {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman");
        let ok = CmdOutput {
            status: 0,
            stdout: "c0ffee\n".into(),
            stderr: String::new(),
        };
        match args[0].as_str() {
            "ps" => Ok(CmdOutput {
                status: 0,
                stdout: "[]".into(),
                stderr: String::new(),
            }),
            "run" => {
                let name = flag(args, "--name").unwrap_or_default().to_string();
                let model = args
                    .iter()
                    .find_map(|a| a.strip_prefix("lmgw.model="))
                    .expect("every managed container is labelled with its model")
                    .to_string();
                let port: u16 = flag(args, "-p")
                    .and_then(|p| p.rsplit(':').nth(1))
                    .and_then(|p| p.parse().ok())
                    .expect("every managed container publishes a host port");
                let file = flag(args, "-m")
                    .and_then(|m| m.rsplit('/').next())
                    .unwrap_or_default()
                    .to_string();
                let gate = {
                    let mut w = self.world.lock().unwrap();
                    w.runs.push(model.clone());
                    w.run_gate.clone()
                };
                if let Some(mut gate) = gate {
                    let _ = gate.wait_for(|open| *open).await;
                }
                let mut w = self.world.lock().unwrap();
                if w.fail_run.contains(&model) {
                    return Ok(CmdOutput {
                        status: 125,
                        stdout: String::new(),
                        stderr: format!("Error: {model} failed to start (test)\n"),
                    });
                }
                let bytes = w.files.get(&file).copied().unwrap_or(0);
                // Above any real `pid_max`, so the real `/proc` walk under it
                // finds nothing.
                let pid = 5_000_000 + w.runs.len() as u32;
                w.pids.insert(model.clone(), pid);
                w.size.insert(model.clone(), bytes);
                w.names.insert(name, model.clone());
                w.ports.insert(port, model.clone());
                if args
                    .iter()
                    .any(|a| a == "--mmproj" || a.starts_with("--mmproj="))
                {
                    w.projectors.insert(model.clone());
                }
                w.loaded.insert(model);
                Ok(ok)
            }
            // Per-process attribution's `State.Pid` read (§4.7).
            "inspect" if args.get(2).map(String::as_str) == Some(HOST_PID_FORMAT) => {
                let w = self.world.lock().unwrap();
                let (mut stdout, mut stderr) = (String::new(), String::new());
                for n in &args[3..] {
                    let pid = w
                        .names
                        .get(n)
                        .filter(|m| w.loaded.contains(*m))
                        .and_then(|m| w.pids.get(m));
                    match pid {
                        Some(pid) => stdout.push_str(&format!("{n} {pid}\n")),
                        None => stderr.push_str(&format!("Error: no such object: \"{n}\"\n")),
                    }
                }
                Ok(CmdOutput {
                    status: if stderr.is_empty() { 0 } else { 125 },
                    stdout,
                    stderr,
                })
            }
            "stop" => {
                let gate = self.world.lock().unwrap().stop_gate.clone();
                if let Some(mut gate) = gate {
                    let _ = gate.wait_for(|open| *open).await;
                }
                let name = args.last().cloned().unwrap_or_default();
                let mut w = self.world.lock().unwrap();
                if let Some(model) = w.names.get(&name).cloned() {
                    w.loaded.remove(&model);
                    w.stops.push(model);
                }
                Ok(ok)
            }
            _ => Ok(ok),
        }
    }
}

/// One container: `/health`, `/slots` about whichever model runs on this
/// port, a chat answer naming that model (or llama-server's context refusal,
/// for a model in [`World::refuse_context`]), and the count's
/// `/apply-template` and `/tokenize`. A bare listener, not the pooled one, so
/// dropping the server really closes the port — a dead container.
async fn container(world: Arc<Mutex<World>>) -> MockServer {
    let server = MockServer::builder()
        .listener(std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .start()
        .await;
    let port = server.address().port();
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .mount(&server)
        .await;
    let slots_world = world.clone();
    Mock::given(method("GET"))
        .and(path("/slots"))
        .respond_with(move |_: &Request| {
            let w = slots_world.lock().unwrap();
            let busy = w.ports.get(&port).is_some_and(|m| w.busy.contains(m));
            ResponseTemplate::new(200).set_body_json(json!([{
                "id": 0, "is_processing": busy, "id_task": 0,
            }]))
        })
        .mount(&server)
        .await;
    let chat_world = world.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &Request| {
            let mut w = chat_world.lock().unwrap();
            let model = w.ports.get(&port).cloned().unwrap_or_default();
            if w.refuse_context.contains(&model) {
                return ResponseTemplate::new(400).set_body_json(json!({"error": {
                    "code": 400, "type": "exceed_context_size_error",
                    "message": "request (5000 tokens) exceeds the available context size",
                    "n_prompt_tokens": 5000, "n_ctx": 4096,
                }}));
            }
            w.chats.push(model.clone());
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "c", "object": "chat.completion", "created": 0, "model": model,
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": format!("ok from {model}")}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            }))
        })
        .mount(&server)
        .await;
    let template_world = world.clone();
    Mock::given(method("POST"))
        .and(path("/apply-template"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let has = |kind: &str| {
                body["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["content"].as_array())
                    .flatten()
                    .any(|p| p["type"] == kind)
            };
            let (image, audio) = (has("image_url"), has("input_audio"));
            let mut w = template_world.lock().unwrap();
            let model = w.ports.get(&port).cloned().unwrap_or_default();
            let seeing = w.projectors.contains(&model);
            w.templated.push(body);
            // llama-server's own words (`oaicompat_chat_params_parse`), thrown
            // before anything is rendered and answered as a server error.
            let refused = if image && !seeing {
                Some(
                    "image input is not supported - hint: if this is unexpected, you may need \
                      to provide the mmproj",
                )
            } else if audio {
                Some(
                    "audio input is not supported - hint: if this is unexpected, you may need \
                      to provide the mmproj",
                )
            } else {
                None
            };
            match refused {
                Some(message) => ResponseTemplate::new(500).set_body_json(json!({"error": {
                    "code": 500, "message": message, "type": "server_error",
                }})),
                None => ResponseTemplate::new(200).set_body_json(json!({"prompt": "p"})),
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(move |req: &Request| {
            let mut w = world.lock().unwrap();
            w.tokenized
                .push(serde_json::from_slice(&req.body).unwrap_or_default());
            let n = w.prompt_tokens as usize;
            ResponseTemplate::new(200).set_body_json(json!({"tokens": vec![1; n]}))
        })
        .mount(&server)
        .await;
    server
}

/// The world, the gateway state, and the containers still alive.
pub struct Gpu {
    pub state: SharedState,
    pub world: Arc<Mutex<World>>,
    containers: Arc<Mutex<Vec<MockServer>>>,
    /// The cloud upstreams [`Self::cloud`] made, kept alive with the world.
    clouds: Mutex<Vec<MockServer>>,
    dir: tempfile::TempDir,
}

impl Gpu {
    /// A card of `total` bytes and `containers` ports for starts, one per
    /// start. Admission on, no headroom, a queue timeout of `queue_seconds`.
    pub async fn new(total: u64, containers: usize, queue_seconds: u64) -> Self {
        let state = AppState::init_for_tests().await.unwrap();
        let world = Arc::new(Mutex::new(World::default()));
        let dir = tempfile::tempdir().unwrap();
        let mut servers = Vec::with_capacity(containers);
        for _ in 0..containers {
            servers.push(container(world.clone()).await);
        }
        let ports = Arc::new(Mutex::new(
            servers
                .iter()
                .map(|c| c.address().port())
                .collect::<VecDeque<u16>>(),
        ));
        state.set_runtime_for_tests(Arc::new(Registry::with_ports(
            Arc::new(Podman {
                world: world.clone(),
            }),
            reqwest::Client::new(),
            Arc::new(move || {
                ports.lock().unwrap().pop_front().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::AddrNotAvailable,
                        "test allocator ran out of ports — an unexpected extra start",
                    )
                })
            }),
        )));
        let mut s = Settings::default();
        s.router.models_dir = dir.path().display().to_string();
        s.vram.enabled = true;
        s.vram.headroom_mb = 0;
        s.vram.queue_timeout_seconds = queue_seconds;
        store::save_settings(&state.db, &s).await.unwrap();
        state.reload_snapshot().await.unwrap();
        state.vram.set_probe(Arc::new(FakeGpu {
            total,
            world: world.clone(),
        }));
        Self {
            state,
            world,
            containers: Arc::new(Mutex::new(servers)),
            clouds: Mutex::new(Vec::new()),
            dir,
        }
    }

    pub fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap()
    }

    /// The chat models dir the rows' files live in.
    pub fn models_dir(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Overwrite `name` in the models dir with a header-only GGUF (no
    /// tensors, no chat template) — a readable file, where the rows' sparse
    /// weights are not GGUFs at all.
    pub fn write_gguf(&self, name: &str) {
        fn put_str(buf: &mut Vec<u8>, s: &str) {
            buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        const VT_STRING: u32 = 8;
        let mut out = Vec::new();
        out.extend_from_slice(b"GGUF");
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&1u64.to_le_bytes());
        put_str(&mut out, "general.architecture");
        out.extend_from_slice(&VT_STRING.to_le_bytes());
        put_str(&mut out, "qwen3");
        std::fs::write(self.dir.path().join(name), out).unwrap();
    }

    /// Hold every `podman run` from now on until the returned sender says
    /// `true` ([`World::run_gate`]).
    pub fn gate_runs(&self) -> tokio::sync::watch::Sender<bool> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        self.world().run_gate = Some(rx);
        tx
    }

    /// Hold every `podman stop` from now on until the returned sender says
    /// `true` ([`World::stop_gate`]).
    pub fn gate_stops(&self) -> tokio::sync::watch::Sender<bool> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        self.world().stop_gate = Some(rx);
        tx
    }

    pub fn file(&self, name: &str, bytes: u64) {
        std::fs::File::create(self.dir.path().join(name))
            .unwrap()
            .set_len(bytes)
            .unwrap();
        self.world().files.insert(name.into(), bytes);
    }

    /// A public local chat row on a weights file of `bytes`.
    pub async fn model(&self, id: &str, bytes: u64) {
        self.ladder(id, bytes, &[]).await;
    }

    /// A chat row whose base is `bytes` and whose higher rungs are
    /// `(bytes, ctx)` each — one slot, a max output of 16, the base at
    /// `-c 64`.
    pub async fn ladder(&self, id: &str, bytes: u64, rungs: &[(u64, i64)]) {
        let base = format!("{id}.gguf");
        self.file(&base, bytes);
        let mut ladder = Vec::new();
        for (i, (b, ctx)) in rungs.iter().enumerate() {
            let f = format!("{id}-{}.gguf", i + 2);
            self.file(&f, *b);
            ladder.push(Rung {
                gguf_path: f,
                ctx_size: *ctx,
            });
        }
        let params = if rungs.is_empty() {
            LlamaParams::default()
        } else {
            LlamaParams {
                ctx_size: Some(64),
                parallel: Some(1),
                n_predict: Some(16),
                ..Default::default()
            }
        };
        store::insert_local_model(
            &self.state.db,
            &NewLocalModel {
                model_id: id.into(),
                gguf_path: base,
                params,
                args: vec![],
                idle_seconds: 0,
                enabled: true,
                public: true,
                image: None,
                extra_run_args: None,
                warm_start: false,
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
                capabilities_override: None,
                ladder,
            },
        )
        .await
        .unwrap();
        self.state.reload_snapshot().await.unwrap();
    }

    /// A background candidate alias, stored directly (the save-time rules are
    /// `candidate_aliases.rs`'s), with no fallback.
    pub async fn background_alias(&self, alias: &str, candidates: &[&str]) {
        store::insert_candidate_alias(
            &self.state.db,
            &NewCandidateAlias {
                alias: alias.into(),
                candidates: candidates.iter().map(|c| c.to_string()).collect(),
                background: true,
                fallback_mode: HoldFallbackMode::None,
                fallback: None,
                capabilities_disabled: vec![],
                capabilities_enabled: vec![],
                enabled: true,
                notes: String::new(),
            },
        )
        .await
        .unwrap();
        self.state.reload_snapshot().await.unwrap();
    }

    /// A chat row stored as given, on a weights file of `bytes` named by its
    /// `gguf_path` — for a test that sets fields [`Self::ladder`] does not
    /// (capabilities, a row fallback).
    pub async fn row(&self, m: NewLocalModel, bytes: u64) {
        self.file(&m.gguf_path, bytes);
        store::insert_local_model(&self.state.db, &m).await.unwrap();
        self.state.reload_snapshot().await.unwrap();
    }

    /// A candidate alias stored as given (the save-time rules are
    /// `candidate_aliases.rs`'s).
    pub async fn candidate(&self, a: NewCandidateAlias) {
        store::insert_candidate_alias(&self.state.db, &a)
            .await
            .unwrap();
        self.state.reload_snapshot().await.unwrap();
    }

    /// A plain alias `name` on a cloud upstream of its own (a wiremock that
    /// answers every chat request with "from the cloud"), with the owner's
    /// `capabilities_override` when given.
    pub async fn cloud(&self, name: &str, capabilities_override: Option<serde_json::Value>) {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "c", "object": "chat.completion", "created": 0, "model": "gpt",
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "from the cloud"}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })))
            .mount(&mock)
            .await;
        let up = store::insert_upstream(
            &self.state.db,
            &NewUpstream {
                name: format!("cloud-{name}"),
                protocol: Protocol::Openai,
                kind: UpstreamKind::Generic,
                base_url: format!("{}/v1", mock.uri()),
                api_key: None,
                extra_headers: vec![],
                timeout_ms: 5_000,
                enabled: true,
                expose_all: false,
                expose_prefix: String::new(),
                supports_responses: false,
            },
        )
        .await
        .unwrap();
        store::insert_alias(
            &self.state.db,
            &NewAlias {
                alias: name.into(),
                upstream_id: up,
                upstream_model_id: "gpt".into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override,
            },
        )
        .await
        .unwrap();
        self.state.reload_snapshot().await.unwrap();
        self.clouds.lock().unwrap().push(mock);
    }

    /// The route a local model's own name resolves to.
    pub fn route(&self, id: &str) -> Route {
        self.state.snapshot().resolve(id).unwrap()
    }

    pub fn runs(&self) -> Vec<String> {
        self.world().runs.clone()
    }

    pub fn stops(&self) -> Vec<String> {
        self.world().stops.clone()
    }

    /// Kill the container on `port`: its server is dropped, so the port
    /// refuses connections while the registry still believes it is up.
    pub async fn kill(&self, port: u16) {
        let dead = {
            let mut cs = self.containers.lock().unwrap();
            let i = cs
                .iter()
                .position(|c| c.address().port() == port)
                .expect("a container this world made");
            cs.remove(i)
        };
        drop(dead);
        for _ in 0..500 {
            if reqwest::Client::new()
                .get(format!("http://127.0.0.1:{port}/health"))
                .timeout(Duration::from_millis(50))
                .send()
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the killed container is still answering on {port}");
    }
}
