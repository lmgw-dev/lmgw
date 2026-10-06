//! The per-model container registry and `acquire` (design §3.2, §3.5, §3.6,
//! §10.3–10.4).
//!
//! Everything here runs against a fake `CommandRunner` (no podman is
//! executed) and a wiremock `/health` (the port allocator is pointed straight
//! at the mock server, which is what makes "the container came up on port P"
//! a real HTTP fact rather than a stub). The properties under test are the
//! concurrency contract — one start per model, N starts for N models, an
//! in-flight claim that no stop can slip past — and the two failure paths
//! that only ever show up on a real box: a port taken between allocation and
//! `podman run` (§10.4) and a container that never answers `/health`.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::config::{ImageModel, LlamaParams};
use lmgw_core::runtime::argv::LlamaArgs;
use lmgw_core::runtime::descriptor::ModelRuntime;
use lmgw_core::runtime::descriptor::RungPos;
use lmgw_core::runtime::registry::{
    llama_server_candidates, AcquireSpec, ClaimStatus, CmdOutput, CommandRunner, DrainEnd, Marked,
    Registry, RuntimeError, RuntimeState, StartSpec,
};
use lmgw_core::runtime::{container_name, Class};
use tokio::sync::{watch, Barrier};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod start_races;

// ---------------------------------------------------------------------------
// The fake podman
// ---------------------------------------------------------------------------

enum Reply {
    /// podman ran and said this.
    Out(CmdOutput),
    /// podman could not be executed at all (missing binary, fork failure).
    Spawn(String),
}

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    /// Consumed one per `podman run`, in order; exhausted = success.
    run_replies: Mutex<VecDeque<Reply>>,
    /// What `podman logs` returns.
    logs: Mutex<String>,
    /// When set, every `podman run` parks until the test flips it — the seam
    /// that lets a test hold a start open and prove what a *second* acquire
    /// does while it is in flight.
    gate: Mutex<Option<watch::Receiver<bool>>>,
    /// When set, every `podman run` waits here. A barrier of 2 only releases
    /// if two starts are genuinely in flight at the same moment.
    barrier: Mutex<Option<Arc<Barrier>>>,
    /// What a throwaway `--help` container prints — the sd-server flag
    /// vocabulary probe (image-generation design §2.6). `None` makes the
    /// probe fail, which is the "image not pulled / GPU busy" case.
    help: Mutex<Option<String>>,
    /// What `podman inspect` prints; `None` keeps the old reply (`c0ffee`,
    /// which no JSON reader accepts — "podman could not say").
    inspect: Mutex<Option<String>>,
    /// When set, `podman inspect` answers "no such container": the container
    /// was removed from under the registry.
    inspect_gone: Mutex<bool>,
    /// What `podman image inspect <ref>` prints, per reference: the ID and
    /// entrypoint every `--help` read resolves before it looks at its cache.
    /// Consumed front to back with the last reply sticking, so a test can
    /// retag an image between two reads. A reference with no entry answers
    /// `c0ffee` like every other verb — one ID for every image, which the
    /// sd-server tests (one image each) never notice.
    images: Mutex<HashMap<String, VecDeque<CmdOutput>>>,
    /// When set, every `podman stop` exits non-zero — a container that
    /// ignores the stop, a podman that cannot reach its socket.
    fail_stop: Mutex<bool>,
    /// Consumed one per `podman wait`, in order: `Some` parks that wait until
    /// the test flips it, `None` (or an empty queue) answers at once. What
    /// holds a stop *in progress* — the entry `stopping`, the container not
    /// yet gone — while something else runs.
    wait_gates: Mutex<VecDeque<Option<watch::Receiver<bool>>>>,
}

/// `podman image inspect` for an image that is on the box: its ID, then its
/// entrypoint as the JSON podman prints (`null` when unset).
fn image_is(id: &str, entrypoint: &str) -> CmdOutput {
    CmdOutput {
        status: 0,
        stdout: format!("{id} {entrypoint}\n"),
        stderr: String::new(),
    }
}

/// `podman image inspect` (or `run --pull never`) for an image that is not.
fn image_unknown(image: &str) -> CmdOutput {
    CmdOutput {
        status: 125,
        stdout: String::new(),
        stderr: format!("Error: {image}: image not known\n"),
    }
}

/// `podman run … --entrypoint /sd-server <image> --help`: the image class's
/// vocabulary probe (image-generation design §2.6).
fn is_sdcpp_help(argv: &[String]) -> bool {
    argv.iter().any(|a| a == "--help")
        && argv
            .windows(2)
            .any(|w| w[0] == "--entrypoint" && w[1] == "/sd-server")
}

fn success() -> CmdOutput {
    CmdOutput {
        status: 0,
        stdout: "c0ffee\n".into(),
        stderr: String::new(),
    }
}

#[async_trait::async_trait]
impl CommandRunner for Fake {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman", "the registry only ever shells podman");
        // Recorded before any awaiting, so a test can observe that a start is
        // in flight while it is still parked on the gate.
        self.calls.lock().unwrap().push(args.to_vec());
        match args[0].as_str() {
            // The sd-server vocabulary probe is a `run` too, but a
            // throwaway one: it must not consume a queued reply meant for a
            // real start. Matched on its entrypoint, so the llama help read
            // (which the tests below drive through `run_replies`) is
            // untouched.
            "run" if is_sdcpp_help(args) => {
                let help = self.help.lock().unwrap().clone();
                Ok(match help {
                    Some(text) => CmdOutput {
                        status: 0,
                        stdout: text,
                        stderr: String::new(),
                    },
                    None => CmdOutput {
                        status: 127,
                        stdout: String::new(),
                        stderr: "error while loading shared libraries: libcuda.so.1".into(),
                    },
                })
            }
            "run" => {
                let gate = self.gate.lock().unwrap().clone();
                if let Some(mut rx) = gate {
                    while !*rx.borrow_and_update() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                }
                let barrier = self.barrier.lock().unwrap().clone();
                if let Some(b) = barrier {
                    b.wait().await;
                }
                let reply = self.run_replies.lock().unwrap().pop_front();
                match reply {
                    Some(Reply::Spawn(msg)) => {
                        Err(std::io::Error::new(std::io::ErrorKind::NotFound, msg))
                    }
                    Some(Reply::Out(out)) => Ok(out),
                    None => Ok(success()),
                }
            }
            "logs" => Ok(CmdOutput {
                status: 0,
                stdout: self.logs.lock().unwrap().clone(),
                stderr: String::new(),
            }),
            "wait" => {
                let gate = self.wait_gates.lock().unwrap().pop_front().flatten();
                if let Some(mut rx) = gate {
                    while !*rx.borrow_and_update() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                }
                Ok(success())
            }
            "stop" if *self.fail_stop.lock().unwrap() => Ok(CmdOutput {
                status: 1,
                stdout: String::new(),
                stderr: "Error: container is not stopping\n".into(),
            }),
            "image" if args[1] == "inspect" => {
                let image = args.last().unwrap();
                let mut images = self.images.lock().unwrap();
                Ok(match images.get_mut(image) {
                    Some(q) if q.len() > 1 => q.pop_front().unwrap(),
                    Some(q) => q.front().cloned().unwrap_or_else(success),
                    None => success(),
                })
            }
            "inspect" if *self.inspect_gone.lock().unwrap() => Ok(CmdOutput {
                status: 125,
                stdout: String::new(),
                stderr: format!(
                    "Error: no such container {}\n",
                    args.last().cloned().unwrap_or_default()
                ),
            }),
            "inspect" => Ok(match self.inspect.lock().unwrap().clone() {
                Some(json) => CmdOutput {
                    status: 0,
                    stdout: json,
                    stderr: String::new(),
                },
                None => success(),
            }),
            _ => Ok(success()),
        }
    }
}

impl Fake {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    /// What `podman image inspect <image>` answers from now on, in order.
    fn set_image(&self, image: &str, replies: Vec<CmdOutput>) {
        self.images
            .lock()
            .unwrap()
            .insert(image.to_string(), VecDeque::from(replies));
    }

    /// The `--entrypoint` of every non-sd-server `run`, in order — which
    /// binaries a llama `--help` read tried.
    fn entrypoints(&self) -> Vec<String> {
        self.runs()
            .iter()
            .filter_map(|argv| {
                let i = argv.iter().position(|a| a == "--entrypoint")?;
                argv.get(i + 1).cloned()
            })
            .collect()
    }

    /// The podman subcommand of every call, in order.
    fn verbs(&self) -> Vec<String> {
        self.calls().iter().map(|c| c[0].clone()).collect()
    }

    fn runs(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c[0] == "run" && !is_sdcpp_help(c))
            .collect()
    }

    /// The throwaway sd-server `--help` runs, which `runs()` deliberately
    /// excludes.
    fn help_runs(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c[0] == "run" && is_sdcpp_help(c))
            .collect()
    }

    /// The host port each `podman run` published, read back out of `-p`.
    fn published_ports(&self) -> Vec<u16> {
        self.runs()
            .iter()
            .map(|argv| {
                let i = argv.iter().position(|a| a == "-p").expect("no -p in argv");
                argv[i + 1]
                    .rsplit(':')
                    .nth(1)
                    .unwrap()
                    .parse()
                    .expect("unparseable -p")
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A wiremock that answers `/health` with 200, standing in for a container
/// that finished loading (§10.3).
async fn healthy() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .mount(&server)
        .await;
    server
}

/// A registry whose port allocator hands out exactly `ports`, in order — see
/// `PortAllocator`. Running out is a panic-worthy test bug, not a scenario.
fn registry(runner: Arc<Fake>, ports: Vec<u16>) -> Arc<Registry> {
    let queue = Arc::new(Mutex::new(VecDeque::from(ports)));
    Arc::new(Registry::with_ports(
        runner,
        reqwest::Client::new(),
        Arc::new(move || {
            queue.lock().unwrap().pop_front().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "test allocator ran out of ports — an unexpected extra start",
                )
            })
        }),
    ))
}

fn runtime(class: Class, model_id: &str) -> ModelRuntime {
    ModelRuntime {
        class,
        model_id: model_id.into(),
        image: "localhost/llama-server-cuda:official-latest".into(),
        extra_run_args: vec!["--device".into(), "nvidia.com/gpu=all".into()],
        idle_seconds: 0,
        warm_start: false,
        enabled: true,
        llama: Some(LlamaArgs::Chat {
            gguf_path: format!("{model_id}.gguf"),
            params: Box::default(),
            args: vec![],
        }),
        audio: None,
        audio_settings: None,
        image_model: None,
        sdcpp_caps: None,
        rung: None,
    }
}

/// Never read for a llama runtime (`render_spec` only touches `data_dir` for
/// audio) — a path that does not exist is deliberate, so a test that somehow
/// started depending on it would fail loudly instead of silently reading a
/// real directory.
fn spec<'a>(rt: &'a ModelRuntime, load_ms: u64) -> AcquireSpec<'a> {
    AcquireSpec {
        runtime: rt,
        container_prefix: "lmgw",
        models_dir: "/srv/models",
        data_dir: Path::new("/nonexistent/lmgw-test-data-dir"),
        may_write_models_dir: true,
        load_timeout: Duration::from_millis(load_ms),
        stop_timeout: Duration::from_millis(500),
    }
}

/// Spin until `cond`, in millisecond steps. Bounded so a broken invariant
/// fails the suite instead of hanging it.
async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..3_000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Let a just-spawned task run to its next await point. The test runtime is
/// single-threaded, so this is deterministic: after these yields a spawned
/// acquire has provably parked on the entry it found.
async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

// ---------------------------------------------------------------------------
// Cold start
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cold_acquire_starts_the_container_and_hands_back_its_endpoint() {
    let health = healthy().await;
    let port = health.address().port();
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![port]);

    let rt = runtime(Class::Chat, "qwen3-2b");
    let guard = reg.acquire(&spec(&rt, 5_000)).await.unwrap();

    assert_eq!(guard.endpoint(), format!("http://127.0.0.1:{port}"));
    assert_eq!(guard.class(), Class::Chat);
    assert_eq!(guard.model_id(), "qwen3-2b");

    let runs = fake.runs();
    assert_eq!(runs.len(), 1, "one acquire, one podman run");
    let argv = &runs[0];
    let name = container_name("lmgw", Class::Chat, "qwen3-2b");
    assert!(argv.contains(&"--replace".to_string()));
    assert!(argv.contains(&name));
    assert!(argv
        .windows(2)
        .any(|w| w[0] == "-p" && w[1] == format!("127.0.0.1:{port}:8080")));

    let view = reg.list();
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].state, RuntimeState::Ready);
    assert_eq!(view[0].in_flight, 1, "the guard holds a claim");
    assert_eq!(view[0].port, port);
    assert_eq!(view[0].container_name, name);
}

#[tokio::test]
async fn a_warm_model_is_reused_without_a_second_run() {
    let health = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![health.address().port()]);
    let rt = runtime(Class::Chat, "warm");

    let first = reg.acquire(&spec(&rt, 5_000)).await.unwrap();
    let second = reg.acquire(&spec(&rt, 5_000)).await.unwrap();

    assert_eq!(first.endpoint(), second.endpoint());
    assert_eq!(fake.runs().len(), 1);
    assert_eq!(reg.list()[0].in_flight, 2);
}

// ---------------------------------------------------------------------------
// Concurrency: one start per model, N starts for N models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_acquires_of_one_model_start_it_exactly_once() {
    let health = healthy().await;
    let port = health.address().port();
    let (open, gate) = watch::channel(false);
    let fake = Arc::new(Fake::default());
    *fake.gate.lock().unwrap() = Some(gate);
    // One port only: a second start would fail in the allocator, so "exactly
    // one run" is enforced twice over.
    let reg = registry(fake.clone(), vec![port]);

    let first = tokio::spawn({
        let reg = reg.clone();
        async move {
            let rt = runtime(Class::Chat, "shared");
            // The guard comes back with the endpoint: dropping it here would
            // release the claim the assertion below is about.
            reg.acquire(&spec(&rt, 5_000))
                .await
                .map(|g| (g.endpoint(), g))
        }
    });
    wait_for("the first start to reach podman run", || {
        !fake.runs().is_empty()
    })
    .await;

    let second = tokio::spawn({
        let reg = reg.clone();
        async move {
            let rt = runtime(Class::Chat, "shared");
            // The guard comes back with the endpoint: dropping it here would
            // release the claim the assertion below is about.
            reg.acquire(&spec(&rt, 5_000))
                .await
                .map(|g| (g.endpoint(), g))
        }
    });
    settle().await;
    assert_eq!(
        fake.runs().len(),
        1,
        "the second acquire must park on the first start, not launch its own"
    );

    open.send_replace(true);
    let (a, b) = tokio::join!(first, second);
    let (a, b) = (a.unwrap().unwrap(), b.unwrap().unwrap());
    assert_eq!(a.0, format!("http://127.0.0.1:{port}"));
    assert_eq!(b.0, a.0);
    assert_eq!(fake.runs().len(), 1);
    assert_eq!(reg.list()[0].in_flight, 2, "both guards hold a claim");
}

#[tokio::test]
async fn starts_of_different_models_overlap() {
    // The §4 fix, pinned: the map lock is never held across the start, so two
    // models load at once. The barrier is the assertion — it only releases
    // when both `podman run`s are in flight simultaneously, so a registry
    // that serialized starts would hang here, and the timeout turns that hang
    // into a failure.
    let alpha = healthy().await;
    let beta = healthy().await;
    let fake = Arc::new(Fake::default());
    *fake.barrier.lock().unwrap() = Some(Arc::new(Barrier::new(2)));
    let reg = registry(
        fake.clone(),
        vec![alpha.address().port(), beta.address().port()],
    );

    let one = tokio::spawn({
        let reg = reg.clone();
        async move {
            let rt = runtime(Class::Chat, "alpha");
            reg.acquire(&spec(&rt, 5_000)).await.map(|g| (g.port(), g))
        }
    });
    let two = tokio::spawn({
        let reg = reg.clone();
        async move {
            let rt = runtime(Class::Aux, "beta");
            reg.acquire(&spec(&rt, 5_000)).await.map(|g| (g.port(), g))
        }
    });

    let joined = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(one, two) })
        .await
        .expect("the two starts never overlapped — they were serialized");
    let a = joined.0.unwrap().unwrap();
    let b = joined.1.unwrap().unwrap();

    assert_ne!(a.0, b.0, "each model got its own port");
    assert_eq!(fake.runs().len(), 2);
    assert_eq!(reg.list().len(), 2);
    assert!(reg
        .list()
        .iter()
        .all(|v| v.state == RuntimeState::Ready && v.in_flight == 1));
}

// ---------------------------------------------------------------------------
// Failed starts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_start_carries_the_log_tail_and_leaves_nothing_behind() {
    let health = healthy().await;
    let fake = Arc::new(Fake::default());
    fake.run_replies
        .lock()
        .unwrap()
        .push_back(Reply::Spawn("podman: no such file or directory".into()));
    *fake.logs.lock().unwrap() =
        "ggml_cuda_init: found 1 CUDA device\nCUDA error: out of memory\nexiting\n".into();
    // A port nothing listens on for the doomed attempt, then the mock's.
    let reg = registry(fake.clone(), vec![1, health.address().port()]);

    let rt = runtime(Class::Chat, "boom");
    let err = reg.acquire(&spec(&rt, 5_000)).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("no such file or directory"), "{msg}");
    assert!(
        msg.contains("CUDA error: out of memory"),
        "the error must carry the container's own log tail: {msg}"
    );
    assert!(err.logs().iter().any(|l| l.contains("CUDA error")));
    assert!(fake.verbs().contains(&"logs".to_string()));
    assert!(
        reg.list().is_empty(),
        "a failed start must not leave a claimed entry behind"
    );

    // And the next acquire starts fresh rather than inheriting the failure.
    let guard = reg.acquire(&spec(&rt, 5_000)).await.unwrap();
    assert_eq!(guard.port(), health.address().port());
    assert_eq!(fake.runs().len(), 2);
}

#[tokio::test]
async fn waiters_on_a_failing_start_are_told_why_and_do_not_pile_on() {
    let (open, gate) = watch::channel(false);
    let fake = Arc::new(Fake::default());
    *fake.gate.lock().unwrap() = Some(gate);
    fake.run_replies
        .lock()
        .unwrap()
        .push_back(Reply::Out(CmdOutput {
            status: 125,
            stdout: String::new(),
            stderr: "Error: short-name resolution enforced but cannot prompt".into(),
        }));
    *fake.logs.lock().unwrap() = "failed to load model\n".into();
    let reg = registry(fake.clone(), vec![1, 2]);

    let first = tokio::spawn({
        let reg = reg.clone();
        async move {
            let rt = runtime(Class::Chat, "doomed");
            reg.acquire(&spec(&rt, 5_000)).await.map(|_| ())
        }
    });
    wait_for("the start to reach podman run", || !fake.runs().is_empty()).await;
    let second = tokio::spawn({
        let reg = reg.clone();
        async move {
            let rt = runtime(Class::Chat, "doomed");
            reg.acquire(&spec(&rt, 5_000)).await.map(|_| ())
        }
    });
    settle().await;
    open.send_replace(true);

    let (a, b) = tokio::join!(first, second);
    let a = a.unwrap().unwrap_err().to_string();
    let b = b.unwrap().unwrap_err().to_string();
    assert!(a.contains("short-name resolution"), "{a}");
    assert!(
        b.contains("short-name resolution"),
        "the waiter must get the failure, not a silent second attempt: {b}"
    );
    assert_eq!(fake.runs().len(), 1, "one doomed start, not two");
    assert!(reg.list().is_empty());
}

#[tokio::test]
async fn a_port_conflict_removes_the_husk_and_retries_once_on_a_fresh_port() {
    let health = healthy().await;
    let good = health.address().port();
    let doomed = if good == 65000 { 64999 } else { 65000 };
    let fake = Arc::new(Fake::default());
    // Measured shape (§10.4): synchronous failure, exit 126, pasta naming the
    // port — and a `created` husk left behind.
    fake.run_replies
        .lock()
        .unwrap()
        .push_back(Reply::Out(CmdOutput {
            status: 126,
            stdout: String::new(),
            stderr: format!(
                "Error: rootlessport cannot expose privileged port {doomed}: cannot listen on the \
             TCP port: listen tcp4 :{doomed}: bind: address already in use"
            ),
        }));
    let reg = registry(fake.clone(), vec![doomed, good]);

    let rt = runtime(Class::Chat, "raced");
    let guard = reg.acquire(&spec(&rt, 5_000)).await.unwrap();

    assert_eq!(guard.port(), good);
    assert_eq!(
        fake.verbs(),
        vec!["run", "rm", "run"],
        "husk collected between the two attempts, and exactly one retry"
    );
    let name = container_name("lmgw", Class::Chat, "raced");
    assert_eq!(fake.calls()[1], vec!["rm", "-f", &name]);
    assert_eq!(fake.published_ports(), vec![doomed, good]);
}

#[tokio::test]
async fn a_container_that_never_reports_healthy_fails_the_start() {
    // A mock with no `/health` route answers 404 forever — one of the
    // "still starting" outcomes (§10.3), so the poll runs to the deadline.
    let silent = MockServer::start().await;
    let fake = Arc::new(Fake::default());
    *fake.logs.lock().unwrap() = "llama_model_load: error loading model\n".into();
    let reg = registry(fake.clone(), vec![silent.address().port()]);

    let rt = runtime(Class::Chat, "stuck");
    let err = reg.acquire(&spec(&rt, 150)).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("vram.load_timeout_seconds"), "{msg}");
    assert!(msg.contains("error loading model"), "{msg}");
    assert!(reg.list().is_empty());

    // The container is *running* at this point — a stuck load, not a crashed
    // one — and no entry names it any more, so nothing else in lmgw would ever
    // stop it: it would sit on the GPU until the next boot reconciliation.
    // Stopped, not removed: the whole value of this failure is in the log tail
    // the error just quoted, and `--replace` collects the husk at the next
    // start (§3.6).
    let name = container_name("lmgw", Class::Chat, "stuck");
    assert_eq!(
        fake.verbs().iter().filter(|v| v.as_str() == "stop").count(),
        1,
        "a start that never went healthy has to stop its container: {:?}",
        fake.calls()
    );
    assert!(
        fake.calls()
            .iter()
            .any(|c| c[0] == "stop" && c.last() == Some(&name)),
        "{:?}",
        fake.calls()
    );
    assert!(
        !fake.verbs().contains(&"rm".to_string()),
        "the container keeps its logs: {:?}",
        fake.calls()
    );
}

#[tokio::test]
async fn a_container_that_exits_while_loading_fails_the_start_at_once() {
    // Measured 2026-09-24: llama-server aborted five seconds into a load that
    // did not fit, the container exited with code 1, and the start still sat
    // out the whole load timeout. The port is silent either way; podman is
    // not.
    let silent = MockServer::start().await;
    let fake = Arc::new(Fake::default());
    *fake.logs.lock().unwrap() =
        "graph_reserve: failed to allocate compute buffers\n[entrypoint] llama-server exited during startup\n".into();
    *fake.inspect.lock().unwrap() =
        Some(r#"[{"State":{"Status":"exited","Running":false,"ExitCode":1}}]"#.into());
    let reg = registry(fake.clone(), vec![silent.address().port()]);

    let rt = runtime(Class::Chat, "crashed");
    let started = std::time::Instant::now();
    // A load timeout far longer than the test may take: only the exit check
    // can end this start in time.
    let err = reg.acquire(&spec(&rt, 120_000)).await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the start waited {:?} for a container that had exited",
        started.elapsed()
    );
    let msg = err.to_string();
    assert!(msg.contains("exited with code 1"), "{msg}");
    assert!(msg.contains("failed to allocate compute buffers"), "{msg}");
    assert!(!msg.contains("vram.load_timeout_seconds"), "{msg}");
    assert!(reg.list().is_empty());
}

#[tokio::test]
async fn a_running_container_is_waited_for_as_before() {
    // podman says it is running: the verdict stays with the deadline.
    let silent = MockServer::start().await;
    let fake = Arc::new(Fake::default());
    *fake.inspect.lock().unwrap() =
        Some(r#"[{"State":{"Status":"running","Running":true,"ExitCode":0}}]"#.into());
    let reg = registry(fake.clone(), vec![silent.address().port()]);
    let rt = runtime(Class::Chat, "slow");
    let err = reg.acquire(&spec(&rt, 2_500)).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("vram.load_timeout_seconds"), "{msg}");
    assert!(
        fake.verbs().iter().any(|v| v == "inspect"),
        "the state was looked at: {:?}",
        fake.calls()
    );
}

// ---------------------------------------------------------------------------
// Stop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_refuses_a_busy_model_unless_forced() {
    let health = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![health.address().port()]);
    let rt = runtime(Class::Chat, "busy");
    let guard = reg.acquire(&spec(&rt, 5_000)).await.unwrap();

    let err = reg.stop(Class::Chat, "busy", false).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::Busy { in_flight: 1, .. }),
        "{err}"
    );
    assert_eq!(reg.list().len(), 1, "a refused stop changes nothing");
    assert!(!fake.verbs().contains(&"stop".to_string()));

    reg.stop(Class::Chat, "busy", true).await.unwrap();
    let name = container_name("lmgw", Class::Chat, "busy");
    assert_eq!(fake.calls()[1], vec!["stop", "-t", "10", &name]);
    assert_eq!(fake.calls()[2], vec!["wait", &name]);
    assert!(reg.list().is_empty());

    // Dropping the guard of a force-stopped model is a no-op, not a panic.
    drop(guard);
    assert!(reg.list().is_empty());
}

#[tokio::test]
async fn dropping_the_guard_releases_the_claim_and_lets_stop_proceed() {
    let health = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![health.address().port()]);
    let rt = runtime(Class::Chat, "polite");

    let guard = reg.acquire(&spec(&rt, 5_000)).await.unwrap();
    assert_eq!(reg.list()[0].in_flight, 1);
    drop(guard);
    assert_eq!(reg.list()[0].in_flight, 0);

    reg.stop(Class::Chat, "polite", false).await.unwrap();
    assert!(reg.list().is_empty());
    assert!(fake.verbs().contains(&"stop".to_string()));
}

#[tokio::test]
async fn stopping_a_model_that_is_not_running_is_a_no_op() {
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![]);
    reg.stop(Class::Aux, "never-started", false).await.unwrap();
    assert!(fake.calls().is_empty(), "nothing to stop, nothing shelled");
}

#[tokio::test]
async fn stop_all_stops_every_entry_and_reports_the_refusals() {
    let alpha = healthy().await;
    let beta = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![alpha.address().port(), beta.address().port()],
    );
    let a = runtime(Class::Chat, "alpha");
    let b = runtime(Class::Aux, "beta");
    let held = reg.acquire(&spec(&a, 5_000)).await.unwrap();
    let held_beta = reg.acquire(&spec(&b, 5_000)).await.unwrap();

    let refused = reg.stop_all(false).await;
    assert_eq!(refused.len(), 2, "both are in flight: {refused:?}");
    assert!(refused
        .iter()
        .all(|e| matches!(e, RuntimeError::Busy { .. })));
    assert_eq!(reg.list().len(), 2);

    drop(held);
    let refused = reg.stop_all(false).await;
    assert_eq!(refused.len(), 1, "beta's guard is still held: {refused:?}");
    assert_eq!(reg.list().len(), 1);

    assert!(
        reg.stop_all(true).await.is_empty(),
        "force overrides the in-flight refusal"
    );
    assert!(reg.list().is_empty());
    drop(held_beta);
}

#[tokio::test]
async fn a_stop_during_a_start_wins_and_nobody_restarts_the_model() {
    // The nastiest ordering in §3.6: `stop` arrives while the container is
    // still coming up, with another acquire already parked on that start.
    // Everybody has to end up with the model *down* — an acquire that quietly
    // restarted it would undo the stop the owner just asked for.
    let health = healthy().await;
    let (open, gate) = watch::channel(false);
    let fake = Arc::new(Fake::default());
    *fake.gate.lock().unwrap() = Some(gate);
    let reg = registry(fake.clone(), vec![health.address().port()]);

    let starter = tokio::spawn({
        let reg = reg.clone();
        async move {
            let rt = runtime(Class::Chat, "doomed-start");
            reg.acquire(&spec(&rt, 5_000)).await.map(|g| (g.port(), g))
        }
    });
    wait_for("the start to reach podman run", || !fake.runs().is_empty()).await;
    let waiter = tokio::spawn({
        let reg = reg.clone();
        async move {
            let rt = runtime(Class::Chat, "doomed-start");
            reg.acquire(&spec(&rt, 5_000)).await.map(|g| (g.port(), g))
        }
    });
    settle().await;

    // Nothing is in flight yet (the entry is only `starting`), so this needs
    // no force.
    reg.stop(Class::Chat, "doomed-start", false).await.unwrap();
    open.send_replace(true);

    let (a, b) = tokio::join!(starter, waiter);
    let a = a.unwrap().unwrap_err();
    let b = b.unwrap().unwrap_err().to_string();
    assert!(
        matches!(a, RuntimeError::Aborted { .. }),
        "the starting task must not hand out a guard for a stopped model: {a}"
    );
    assert!(b.contains("stopped while it was starting"), "{b}");
    assert_eq!(fake.runs().len(), 1, "nobody restarted it");
    assert!(reg.list().is_empty());

    // The container the aborted claim started became healthy a moment after
    // the stop ran, so the stop's own `podman stop` may well have hit nothing.
    // Nothing names that container: it is in no entry, so no reaper, eviction
    // or shutdown could ever reach it. The claim removes it on its way out.
    let name = container_name("lmgw", Class::Chat, "doomed-start");
    assert!(
        fake.calls()
            .iter()
            .any(|c| c[0] == "rm" && c.last() == Some(&name)),
        "the aborted start has to collect the container it started: {:?}",
        fake.calls()
    );
}

// ---------------------------------------------------------------------------
// Claim identity and the LRU stamp (§3.2)
// ---------------------------------------------------------------------------

/// A guard outliving its entry must not touch the entry that replaced it.
///
/// The window is the dead-container path (§3.2): a request holds a claim, the
/// container turns out to be dead, the entry is force-stopped and a fresh one
/// takes the key — and only then does the old guard drop. Releasing by key
/// alone would decrement the *successor's* in-flight count, which is the
/// counter that stops the reaper and the eviction path from taking a model out
/// from under a live request.
#[tokio::test]
async fn a_guard_that_outlived_its_entry_does_not_release_the_successor() {
    let health = healthy().await;
    let port = health.address().port();
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![port, port]);
    let rt = runtime(Class::Chat, "replaced");

    let stale = reg.acquire(&spec(&rt, 5_000)).await.unwrap();
    assert_eq!(reg.list()[0].in_flight, 1);

    // The entry the stale guard was taken on goes away entirely…
    reg.stop(Class::Chat, "replaced", true).await.unwrap();
    assert!(reg.list().is_empty());
    // …and a fresh start takes the same key.
    let live = reg.acquire(&spec(&rt, 5_000)).await.unwrap();
    assert_eq!(reg.list()[0].in_flight, 1);

    drop(stale);
    assert_eq!(
        reg.list()[0].in_flight,
        1,
        "the late guard released a claim it never held on this entry"
    );
    drop(live);
    assert_eq!(reg.list()[0].in_flight, 0);
}

/// `last_used` is the idle reaper's and the LRU's only input, and a request
/// that never reached the container is not evidence the model was used.
///
/// Without this, a dead container is *starved of reaping*: every client retry
/// takes a claim, fails on connect, and re-stamps the entry as freshly used —
/// so the corpse stays in the map (and on the ledger) for a full `idle_seconds`
/// after the last retry, forever if the client keeps retrying.
#[tokio::test]
async fn a_failed_guard_does_not_stamp_the_model_as_freshly_used() {
    let health = healthy().await;
    let port = health.address().port();
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![port, port]);
    let served = runtime(Class::Chat, "served");
    let dead = runtime(Class::Chat, "dead");

    let served_guard = reg.acquire(&spec(&served, 5_000)).await.unwrap();
    let dead_guard = reg.acquire(&spec(&dead, 5_000)).await.unwrap();

    // Real wall clock, because `last_used` is a real `Instant` — the published
    // age is in seconds, so this has to cross one.
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    dead_guard.mark_failed();
    drop(dead_guard);
    drop(served_guard);

    let age = |model: &str| {
        reg.list()
            .into_iter()
            .find(|v| v.model_id == model)
            .expect("entry")
            .last_used_age_seconds
    };
    assert_eq!(
        age("served"),
        0,
        "a served request is what makes a model the most recently used one"
    );
    assert!(
        age("dead") >= 1,
        "a failed request must not refresh the LRU stamp (age {})",
        age("dead")
    );
}

// ---------------------------------------------------------------------------
// Ladder climbs (ladder design §3.4, §12 entries 10 and 19–30)
// ---------------------------------------------------------------------------

/// A two-rung ladder row's descriptor at `index` (0 = the base): its own
/// weights and context, the rest shared — what `model_runtime_at` renders.
fn ladder_runtime(model_id: &str, index: usize) -> ModelRuntime {
    let (file, ctx) = [("base", 64), ("top", 128)][index];
    let mut rt = runtime(Class::Chat, model_id);
    rt.llama = Some(LlamaArgs::Chat {
        gguf_path: format!("{model_id}-{file}.gguf"),
        params: Box::new(LlamaParams {
            ctx_size: Some(ctx),
            parallel: Some(1),
            n_predict: Some(16),
            ..Default::default()
        }),
        args: vec![],
    });
    rt.rung = Some(RungPos { index, of: 2 });
    rt
}

const TOP: RungPos = RungPos { index: 1, of: 2 };

/// The `-m` file of every real `podman run`, in order.
fn run_files(fake: &Fake) -> Vec<String> {
    fake.runs()
        .iter()
        .map(|argv| {
            let i = argv.iter().position(|a| a == "-m").expect("no -m in argv");
            argv[i + 1].rsplit('/').next().unwrap().to_string()
        })
        .collect()
}

fn ticket(marked: Marked) -> lmgw_core::runtime::registry::ClimbTicket {
    match marked {
        Marked::Ticket(t) => t,
        other => panic!("expected the climb ticket, got {other:?}"),
    }
}

/// Mark → drain → start → settle, and what everyone else sees meanwhile:
/// the drain waits for the send in flight and nothing else, an acquire
/// parks on the climb instead of starting the model a second time, and the
/// entry — with every claim counted on it — ends up on the new container,
/// charged at the new rung. A claim taken before the climb learns it moved,
/// and follows without a new acquire.
#[tokio::test]
async fn a_climb_drains_the_sends_then_restarts_the_model_at_the_new_rung() {
    let old = healthy().await;
    let new = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![old.address().port(), new.address().port()],
    );
    let (base, top) = (ladder_runtime("lad", 0), ladder_runtime("lad", 1));

    let mut claim = reg.acquire(&spec(&base, 5_000)).await.unwrap();
    let before = reg.list()[0].clone();
    assert_eq!(before.rung.as_ref().map(|r| (r.rung, r.of)), Some((1, 2)));
    let send = reg
        .begin_send(&claim)
        .expect("the running container takes sends");
    assert_eq!(reg.list()[0].sends, 1);

    let mut climb = ticket(reg.mark_climb(&claim, TOP, "prompt 100 + 16 > 64"));
    let view = reg.list()[0].clone();
    let climbing = view.climbing.as_ref().expect("the mark is on the view");
    assert_eq!((climbing.to, climbing.of), (2, 2), "1-based");
    assert_eq!(climbing.reason, "prompt 100 + 16 > 64");
    assert_eq!(view.state, RuntimeState::Ready, "still serving its send");
    assert!(
        matches!(reg.begin_send(&claim), Err(ClaimStatus::Climbing(_))),
        "a marked model takes no new send"
    );

    // An acquire parks on the climb: no second start, no claim on the old rung.
    let parked = tokio::spawn({
        let reg = reg.clone();
        let base = base.clone();
        async move { reg.acquire(&spec(&base, 5_000)).await }
    });
    // The drain waits for the send in flight, not for the claims.
    let draining = tokio::spawn(async move {
        let drained = climb
            .drain(Some(std::time::Instant::now() + Duration::from_secs(10)))
            .await;
        (drained, climb)
    });
    settle().await;
    assert!(!draining.is_finished(), "the send is still in flight");
    assert!(!parked.is_finished());
    assert_eq!(fake.runs().len(), 1);
    drop(send);
    let (drained, climb) = draining.await.unwrap();
    drained.expect("drained once the send ended");

    let run = climb
        .start(StartSpec::of(&spec(&top, 5_000)))
        .expect("still ours");
    let starting = reg.list()[0].clone();
    assert_eq!(starting.state, RuntimeState::Starting);
    assert_ne!(starting.generation, before.generation, "a new container");
    assert_eq!(
        starting.charge.as_ref().map(|c| c.index),
        Some(1),
        "charged at rung 2 from the claim on"
    );
    run.finish().await.expect("the new rung came up");

    let after = reg.list()[0].clone();
    assert_eq!(after.state, RuntimeState::Ready);
    assert_eq!(after.port, new.address().port());
    assert_eq!(
        after.rung.as_ref().map(|r| (r.rung, r.gguf.as_str())),
        Some((2, "lad-top.gguf"))
    );
    assert!(after.climbing.is_none());
    let parked = parked.await.unwrap().expect("the parked acquire is served");
    assert_eq!(parked.port(), new.address().port());
    assert_eq!(
        after.in_flight, 2,
        "the old claim was carried across, the parked one added"
    );

    // One stop of the old rung, then one start of the new one.
    assert_eq!(run_files(&fake), vec!["lad-base.gguf", "lad-top.gguf"]);
    let verbs = fake.verbs();
    let stop_at = verbs
        .iter()
        .position(|v| v == "stop")
        .expect("the old rung was stopped");
    let runs: Vec<usize> = verbs
        .iter()
        .enumerate()
        .filter(|(_, v)| *v == "run")
        .map(|(i, _)| i)
        .collect();
    assert!(runs[0] < stop_at && stop_at < runs[1], "{verbs:?}");

    // The claim taken before the climb: moved, not gone — and it follows.
    match reg.claim_status(&claim) {
        ClaimStatus::Moved {
            port,
            gate,
            generation,
        } => {
            assert_eq!(port, new.address().port());
            assert_eq!(gate.as_ref().and_then(|g| g.rung), Some(TOP));
            claim.retarget(port, gate, generation);
        }
        other => panic!("the claim should have moved: {other:?}"),
    }
    assert!(matches!(reg.claim_status(&claim), ClaimStatus::Current));
    assert_eq!(claim.port(), new.address().port());
    let _send = reg.begin_send(&claim).expect("and sends there");
}

/// The drain is fail-closed (§12 entry 10): once a mark is set, no send can
/// begin behind it. A ticket dropped before its start — a refused admission,
/// a drain that timed out — clears the mark, and the running rung serves on.
#[tokio::test]
async fn begin_send_fails_closed_after_a_mark_and_a_dropped_ticket_clears_it() {
    let port = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![port.address().port()]);
    let base = ladder_runtime("lad", 0);
    let claim = reg.acquire(&spec(&base, 5_000)).await.unwrap();

    let in_flight = reg.begin_send(&claim).unwrap();
    let mut climb = ticket(reg.mark_climb(&claim, TOP, "why"));
    assert!(matches!(
        reg.begin_send(&claim),
        Err(ClaimStatus::Climbing(_))
    ));

    let parked = tokio::spawn({
        let reg = reg.clone();
        let base = base.clone();
        async move { reg.acquire(&spec(&base, 5_000)).await.map(|g| g.port()) }
    });
    let until = std::time::Instant::now() + Duration::from_millis(50);
    assert_eq!(
        climb.drain(Some(until)).await,
        Err(DrainEnd::Timeout { sends: 1 })
    );
    drop(climb);

    let view = reg.list()[0].clone();
    assert!(view.climbing.is_none(), "the mark went with the ticket");
    assert_eq!(view.state, RuntimeState::Ready);
    assert_eq!(
        parked.await.unwrap().unwrap(),
        port.address().port(),
        "served on the running rung"
    );
    assert_eq!(fake.runs().len(), 1, "nothing was stopped or started");
    assert!(!fake.verbs().contains(&"stop".to_string()));
    drop(in_flight);
    let _next = reg.begin_send(&claim).expect("sends are taken again");
}

/// Two triggers, one reload (§12 races): the second joins the first's mark
/// and raises its target when it needs more, until the start is claimed.
#[tokio::test]
async fn a_joined_trigger_raises_the_target_until_the_start_is_claimed() {
    let port = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![port.address().port()]);
    let rt = ladder_runtime("lad", 0);
    let first = reg.acquire(&spec(&rt, 5_000)).await.unwrap();
    let second = reg.acquire(&spec(&rt, 5_000)).await.unwrap();

    let mid = RungPos { index: 1, of: 3 };
    let top = RungPos { index: 2, of: 3 };
    let climb = ticket(reg.mark_climb(&first, mid, "needs rung 2"));
    match reg.mark_climb(&second, top, "needs rung 3") {
        Marked::Joined { raised, .. } => assert!(raised),
        other => panic!("{other:?}"),
    }
    assert_eq!(climb.to(), top, "one reload serves both");
    assert_eq!(
        reg.list()[0].climbing.as_ref().unwrap().reason,
        "needs rung 3"
    );
    match reg.mark_climb(&second, mid, "a smaller need") {
        Marked::Joined { raised, .. } => assert!(!raised, "never lowered"),
        other => panic!("{other:?}"),
    }
    assert_eq!(climb.to(), top);
}

/// A stop judged on one container never takes a newer one (§12 entry 20):
/// the generation it judged has to be the one running, and not being climbed.
#[tokio::test]
async fn a_generation_checked_stop_spares_a_newer_or_climbing_container() {
    let port = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![port.address().port(), port.address().port()],
    );
    let rt = runtime(Class::Chat, "m");

    drop(reg.acquire(&spec(&rt, 5_000)).await.unwrap());
    let judged = reg.list()[0].generation;
    reg.stop(Class::Chat, "m", true).await.unwrap();
    let claim = reg.acquire(&spec(&rt, 5_000)).await.unwrap();
    let newer = reg.list()[0].generation;
    assert_ne!(judged, newer);

    let err = reg
        .stop_generation(Class::Chat, "m", judged, true)
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Moved { .. }), "{err}");
    assert_eq!(reg.list().len(), 1, "the newer container is untouched");
    assert_eq!(fake.verbs().iter().filter(|v| *v == "stop").count(), 1);

    // Being climbed counts as moved too: the climb replaces it anyway.
    let climb = ticket(reg.mark_climb(&claim, TOP, "why"));
    let err = reg
        .stop_generation(Class::Chat, "m", newer, true)
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Moved { .. }), "{err}");
    drop(climb);

    drop(claim);
    reg.stop_generation(Class::Chat, "m", newer, false)
        .await
        .expect("the judged container, idle: stopped");
    assert!(reg.list().is_empty());
}

/// A forced stop — an override, a delete, the shutdown — wins against a
/// climb (§12 races): the new rung's container is removed when it comes up,
/// the trigger is told, whoever waited on the climb is told it was stopped,
/// and nothing starts the model again.
#[tokio::test]
async fn a_forced_stop_during_a_climb_wins_and_nothing_restarts() {
    let old = healthy().await;
    let new = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![old.address().port(), new.address().port()],
    );
    let (base, top) = (ladder_runtime("lad", 0), ladder_runtime("lad", 1));
    let claim = reg.acquire(&spec(&base, 5_000)).await.unwrap();

    let mut climb = ticket(reg.mark_climb(&claim, TOP, "why"));
    climb.drain(None).await.unwrap();
    let (open, gate) = watch::channel(false);
    *fake.gate.lock().unwrap() = Some(gate);
    let run = climb.start(StartSpec::of(&spec(&top, 5_000))).unwrap();
    wait_for("the new rung's podman run", || fake.runs().len() == 2).await;
    let waiter = tokio::spawn({
        let reg = reg.clone();
        let base = base.clone();
        async move { reg.acquire(&spec(&base, 5_000)).await.map(|g| g.port()) }
    });
    settle().await;

    reg.stop(Class::Chat, "lad", true).await.unwrap();
    open.send_replace(true);
    let outcome = run.finish().await;
    assert!(
        matches!(outcome, Err(RuntimeError::Aborted { .. })),
        "{outcome:?}"
    );
    let told = waiter.await.unwrap().unwrap_err().to_string();
    assert!(
        told.contains("stopped while it was climbing to rung 2/2"),
        "{told}"
    );
    assert!(reg.list().is_empty(), "down, as the stop wanted");
    assert_eq!(fake.runs().len(), 2, "nothing started it again");
    let name = container_name("lmgw", Class::Chat, "lad");
    assert!(
        fake.calls()
            .iter()
            .any(|c| c[0] == "rm" && c.last() == Some(&name)),
        "the container the climb started is collected: {:?}",
        fake.calls()
    );
    assert!(matches!(reg.claim_status(&claim), ClaimStatus::Gone));
}

/// A stop that lands while the climb is still draining ends the drain.
#[tokio::test]
async fn a_forced_stop_during_the_drain_ends_it() {
    let port = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![port.address().port()]);
    let claim = reg
        .acquire(&spec(&ladder_runtime("lad", 0), 5_000))
        .await
        .unwrap();
    let _send = reg.begin_send(&claim).unwrap();
    let mut climb = ticket(reg.mark_climb(&claim, TOP, "why"));
    let draining = tokio::spawn(async move { climb.drain(None).await });
    settle().await;
    reg.stop(Class::Chat, "lad", true).await.unwrap();
    assert_eq!(draining.await.unwrap(), Err(DrainEnd::Gone));
    assert_eq!(fake.runs().len(), 1);
}

/// The new rung will not start (§12 entry 22): the entry is forgotten, the
/// trigger gets the start's error, and whoever waited on the climb is sent
/// back to admission (`ClimbFailed`) — which starts the base, never the
/// broken rung, and never from inside the registry.
#[tokio::test]
async fn a_failed_rung_start_forgets_the_entry_and_the_next_start_is_the_base() {
    let old = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![old.address().port(), 1, old.address().port()],
    );
    let (base, top) = (ladder_runtime("lad", 0), ladder_runtime("lad", 1));
    let claim = reg.acquire(&spec(&base, 5_000)).await.unwrap();

    fake.run_replies
        .lock()
        .unwrap()
        .push_back(Reply::Out(CmdOutput {
            status: 125,
            stdout: String::new(),
            stderr: "Error: lad-top.gguf: no such file".into(),
        }));
    let mut climb = ticket(reg.mark_climb(&claim, TOP, "why"));
    let waiter = tokio::spawn({
        let reg = reg.clone();
        let base = base.clone();
        async move { reg.acquire(&spec(&base, 5_000)).await.map(|g| g.port()) }
    });
    settle().await;
    climb.drain(None).await.unwrap();
    let err = climb
        .start(StartSpec::of(&spec(&top, 5_000)))
        .unwrap()
        .finish()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no such file"), "{err}");
    assert!(reg.list().is_empty(), "forgotten");
    match waiter.await.unwrap() {
        Err(RuntimeError::ClimbFailed { message, .. }) => {
            assert!(message.contains("rung 2/2"), "{message}")
        }
        other => panic!("the waiter goes back to admission: {other:?}"),
    }
    assert!(matches!(reg.claim_status(&claim), ClaimStatus::Gone));

    // Admission's next start renders the base, as every start does.
    let fresh = reg.acquire(&spec(&base, 5_000)).await.unwrap();
    assert_eq!(fresh.port(), old.address().port());
    assert_eq!(
        run_files(&fake),
        vec!["lad-base.gguf", "lad-top.gguf", "lad-base.gguf"]
    );
    assert_eq!(reg.list()[0].rung.as_ref().unwrap().rung, 1);
}

/// Stopping the running rung fails (§12 races): logged, and the new rung is
/// started anyway — `podman run --replace` collects the old container by
/// name.
#[tokio::test]
async fn a_failed_stop_of_the_running_rung_still_climbs() {
    let old = healthy().await;
    let new = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![old.address().port(), new.address().port()],
    );
    let (base, top) = (ladder_runtime("lad", 0), ladder_runtime("lad", 1));
    let claim = reg.acquire(&spec(&base, 5_000)).await.unwrap();

    *fake.fail_stop.lock().unwrap() = true;
    let mut climb = ticket(reg.mark_climb(&claim, TOP, "why"));
    climb.drain(None).await.unwrap();
    climb
        .start(StartSpec::of(&spec(&top, 5_000)))
        .unwrap()
        .finish()
        .await
        .expect("the climb completes");
    let view = reg.list()[0].clone();
    assert_eq!(view.state, RuntimeState::Ready);
    assert_eq!(view.port, new.address().port());
    assert_eq!(view.rung.as_ref().unwrap().rung, 2);
    assert!(fake.verbs().contains(&"stop".to_string()));
    assert!(fake.runs()[1].contains(&"--replace".to_string()));
}

/// [`spec`] whose stops wait as long as a test needs: a stop parked in
/// `podman wait` must not end on its own `unload_timeout` mid-test.
fn patient_spec(rt: &ModelRuntime) -> AcquireSpec<'_> {
    AcquireSpec {
        stop_timeout: Duration::from_secs(30),
        ..spec(rt, 5_000)
    }
}

fn waits(fake: &Fake) -> usize {
    fake.verbs().iter().filter(|v| *v == "wait").count()
}

/// Review finding 2, scenario A (§12 entry 47): a forced stop that is under
/// way — the entry `stopping`, its `podman wait` still running — when the
/// climb's own stop of the old rung returns. The climb has lost: it must not
/// load a rung nobody wants, unaccounted, under the name the stop is taking
/// down.
#[tokio::test]
async fn a_stop_in_progress_keeps_the_climb_from_loading_its_rung() {
    let old = healthy().await;
    let new = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![old.address().port(), new.address().port()],
    );
    let (base, top) = (ladder_runtime("lad", 0), ladder_runtime("lad", 1));
    let claim = reg.acquire(&patient_spec(&base)).await.unwrap();
    let mut climb = ticket(reg.mark_climb(&claim, TOP, "why"));
    climb.drain(None).await.unwrap();

    let (own, own_gate) = watch::channel(false);
    let (owners, owners_gate) = watch::channel(false);
    fake.wait_gates
        .lock()
        .unwrap()
        .extend([Some(own_gate), Some(owners_gate)]);
    let run = climb.start(StartSpec::of(&patient_spec(&top))).unwrap();
    wait_for("the climb's stop of the old rung", || waits(&fake) == 1).await;
    let stopping = tokio::spawn({
        let reg = reg.clone();
        async move { reg.stop(Class::Chat, "lad", true).await }
    });
    wait_for("the owner's stop under way", || {
        waits(&fake) == 2 && reg.list()[0].state == RuntimeState::Stopping
    })
    .await;

    own.send_replace(true);
    let outcome = run.finish().await;
    assert!(
        matches!(outcome, Err(RuntimeError::Aborted { .. })),
        "{outcome:?}"
    );
    assert_eq!(fake.runs().len(), 1, "the new rung was never loaded");

    owners.send_replace(true);
    stopping.await.unwrap().unwrap();
    assert!(reg.list().is_empty());
}

/// Review finding 2, scenario B (§12 entry 47): the new rung's start fails
/// while a forced stop is under way — the stop's doing, not a broken rung.
/// The waiters are told the model was stopped, never `ClimbFailed` (which
/// would send them back to admission to start it again), and the trigger
/// learns it was stopped.
#[tokio::test]
async fn a_start_that_fails_under_a_stop_is_the_stop_not_a_failed_climb() {
    let old = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![old.address().port(), 1]);
    let (base, top) = (ladder_runtime("lad", 0), ladder_runtime("lad", 1));
    let claim = reg.acquire(&patient_spec(&base)).await.unwrap();
    let mut climb = ticket(reg.mark_climb(&claim, TOP, "why"));
    climb.drain(None).await.unwrap();

    let (owners, owners_gate) = watch::channel(false);
    fake.wait_gates
        .lock()
        .unwrap()
        .extend([None, Some(owners_gate)]);
    let (load, load_gate) = watch::channel(false);
    *fake.gate.lock().unwrap() = Some(load_gate);
    fake.run_replies
        .lock()
        .unwrap()
        .push_back(Reply::Out(CmdOutput {
            status: 125,
            stdout: String::new(),
            stderr: "Error: the container was killed".into(),
        }));
    let run = climb.start(StartSpec::of(&patient_spec(&top))).unwrap();
    wait_for("the new rung's podman run", || fake.runs().len() == 2).await;
    let waiter = tokio::spawn({
        let reg = reg.clone();
        let base = base.clone();
        async move { reg.acquire(&patient_spec(&base)).await.map(|g| g.port()) }
    });
    settle().await;
    let stopping = tokio::spawn({
        let reg = reg.clone();
        async move { reg.stop(Class::Chat, "lad", true).await }
    });
    wait_for("the owner's stop under way", || {
        waits(&fake) == 2 && reg.list()[0].state == RuntimeState::Stopping
    })
    .await;

    load.send_replace(true);
    let outcome = run.finish().await;
    assert!(
        matches!(outcome, Err(RuntimeError::Aborted { .. })),
        "{outcome:?}"
    );
    owners.send_replace(true);
    stopping.await.unwrap().unwrap();
    match waiter.await.unwrap() {
        Err(RuntimeError::Start { message, .. }) => {
            assert!(
                message.contains("stopped while it was climbing"),
                "{message}"
            )
        }
        other => panic!("told the model was stopped, not sent to start it: {other:?}"),
    }
    assert!(reg.list().is_empty());
    assert_eq!(fake.runs().len(), 2, "nothing started the model again");
}

/// Review finding 9 (§12 entry 48): the trigger has gone, and a plain stop
/// (an apply, a restart) takes the entry while the new rung loads. A request
/// that arrives *after* the stop began wants the key free, as after any other
/// stop: it waits, then starts the base — it does not inherit the climb's
/// "stopped while climbing".
#[tokio::test]
async fn an_acquire_during_a_stop_of_a_climb_waits_for_the_key() {
    let old = healthy().await;
    let new = healthy().await;
    let fresh = healthy().await;
    let fake = Arc::new(Fake::default());
    let reg = registry(
        fake.clone(),
        vec![
            old.address().port(),
            new.address().port(),
            fresh.address().port(),
        ],
    );
    let (base, top) = (ladder_runtime("lad", 0), ladder_runtime("lad", 1));
    let claim = reg.acquire(&patient_spec(&base)).await.unwrap();
    let mut climb = ticket(reg.mark_climb(&claim, TOP, "why"));
    climb.drain(None).await.unwrap();

    let (owners, owners_gate) = watch::channel(false);
    fake.wait_gates
        .lock()
        .unwrap()
        .extend([None, Some(owners_gate)]);
    let (load, load_gate) = watch::channel(false);
    *fake.gate.lock().unwrap() = Some(load_gate);
    let run = climb.start(StartSpec::of(&patient_spec(&top))).unwrap();
    wait_for("the new rung's podman run", || fake.runs().len() == 2).await;
    drop(claim);
    let stopping = tokio::spawn({
        let reg = reg.clone();
        async move { reg.stop(Class::Chat, "lad", false).await }
    });
    wait_for("the stop under way", || {
        waits(&fake) == 2 && reg.list()[0].state == RuntimeState::Stopping
    })
    .await;
    let arrival = tokio::spawn({
        let reg = reg.clone();
        let base = base.clone();
        async move {
            reg.acquire(&patient_spec(&base))
                .await
                .map(|g| (g.port(), g))
        }
    });
    settle().await;
    assert!(!arrival.is_finished(), "it waits for the key");

    load.send_replace(true);
    assert!(matches!(
        run.finish().await,
        Err(RuntimeError::Aborted { .. })
    ));
    owners.send_replace(true);
    stopping.await.unwrap().unwrap();
    let (port, _guard) = arrival
        .await
        .unwrap()
        .expect("a fresh start, not a failure");
    assert_eq!(port, fresh.address().port());
    assert_eq!(
        run_files(&fake),
        vec!["lad-base.gguf", "lad-top.gguf", "lad-base.gguf"]
    );
}

// ---------------------------------------------------------------------------
// Throwaway probe/help containers (§3.6)
// ---------------------------------------------------------------------------

/// Long enough to pass `help_text`'s "did this produce usable output" bar.
fn help_body(marker: &str) -> String {
    format!(
        "{marker}\n{}",
        "-ctk, --cache-type-k TYPE   KV cache type\n".repeat(40)
    )
}

fn out(stdout: String) -> Reply {
    Reply::Out(CmdOutput {
        status: 0,
        stdout,
        stderr: String::new(),
    })
}

/// Two image IDs as podman prints them (full hex, no `sha256:`).
const ID_A: &str = "aaaaaaaaaaaa0000000000000000000000000000000000000000000000000001";
const ID_B: &str = "bbbbbbbbbbbb0000000000000000000000000000000000000000000000000002";

/// The help read is a *throwaway* container against an image, not an exec into
/// a running one — that is what makes it answerable while every model is
/// stopped, which is the normal state of a per-model install. And it is cached
/// per image, because a per-model image override means there is no single flag
/// vocabulary on the box (§3.6).
#[tokio::test]
async fn help_is_read_from_a_throwaway_container_and_cached_per_image() {
    let fake = Arc::new(Fake::default());
    fake.set_image("localhost/a:latest", vec![image_is(ID_A, "null")]);
    fake.set_image("localhost/b:latest", vec![image_is(ID_B, "null")]);
    *fake.run_replies.lock().unwrap() =
        VecDeque::from(vec![out(help_body("IMAGE-A")), out(help_body("IMAGE-B"))]);
    let reg = registry(fake.clone(), vec![]);

    let a = reg
        .help_text("lmgw", "localhost/a:latest", &[])
        .await
        .unwrap();
    assert!(a.contains("IMAGE-A"));

    let argv = &fake.runs()[0];
    let joined = argv.join(" ");
    assert!(joined.contains("--rm"), "{joined}");
    // The ID the reference resolved to, not the reference: a tag moved between
    // the resolve and the run must not file one image's flags under another's
    // ID.
    assert!(joined.ends_with(&format!("{ID_A} --help")), "{joined}");
    // The entrypoint override is load-bearing: the image's own entrypoint
    // starts a server (nginx in front of llama-server, §10).
    assert!(
        joined.contains("--entrypoint llama-server"),
        "no entrypoint override: {joined}"
    );
    // Nothing is published and nothing is mounted — this container reads a
    // help text, it does not serve or load anything.
    assert!(!argv.iter().any(|a| a == "-p" || a == "-v"), "{joined}");
    // …and it sees no GPU: `--help` needs the libraries, not a device, and a
    // device would cost a CUDA context the VRAM ledger never sees.
    assert!(joined.contains("-e CUDA_VISIBLE_DEVICES="), "{joined}");
    // Named after the reference, so a run abandoned by a timeout can be
    // collected by name.
    assert!(joined.contains("--name lmgw-help-"), "{joined}");
    // A read path must never turn into a multi-gigabyte pull nobody asked for.
    assert!(joined.contains("--pull never"), "{joined}");

    // Cached: a second read of the same image runs no container — it only
    // asks podman which image the reference names now.
    let again = reg
        .help_text("lmgw", "localhost/a:latest", &[])
        .await
        .unwrap();
    assert_eq!(again, a);
    assert_eq!(fake.runs().len(), 1, "{:?}", fake.verbs());
    assert_eq!(
        fake.verbs()
            .iter()
            .filter(|v| v.as_str() == "image")
            .count(),
        2,
        "the ID is resolved on every lookup: {:?}",
        fake.verbs()
    );

    // A different image is a different vocabulary, and gets its own read.
    let b = reg
        .help_text("lmgw", "localhost/b:latest", &[])
        .await
        .unwrap();
    assert!(b.contains("IMAGE-B"), "{b}");
    assert_eq!(fake.runs().len(), 2);
    assert!(
        fake.runs()[1]
            .join(" ")
            .ends_with(&format!("{ID_B} --help")),
        "{:?}",
        fake.runs()[1]
    );
}

/// The bug this keying exists for (container-builds §2.2): `build.sh` retags
/// `official-latest` under a running lmgw. Same reference, new ID — a new
/// vocabulary, read again without anyone invalidating anything. And the old
/// ID keeps its entry, so a rollback to it is a hit, not a re-read.
#[tokio::test]
async fn a_retagged_image_is_read_again_under_its_new_id() {
    let tag = "localhost/llama-server-cuda:official-latest";
    let fake = Arc::new(Fake::default());
    fake.set_image(tag, vec![image_is(ID_A, "[\"/app/entrypoint.sh\"]")]);
    *fake.run_replies.lock().unwrap() = VecDeque::from(vec![
        out(help_body("OLD-BUILD")),
        out(help_body("NEW-BUILD")),
    ]);
    let reg = registry(fake.clone(), vec![]);

    let before = reg.help_text("lmgw", tag, &[]).await.unwrap();
    assert!(before.contains("OLD-BUILD"), "{before}");
    assert!(reg
        .help_text("lmgw", tag, &[])
        .await
        .unwrap()
        .contains("OLD-BUILD"));
    assert_eq!(fake.runs().len(), 1, "unchanged ID, cached");

    // The rebuild lands: the tag now names another image.
    fake.set_image(tag, vec![image_is(ID_B, "[\"/app/entrypoint.sh\"]")]);
    let after = reg.help_text("lmgw", tag, &[]).await.unwrap();
    assert!(
        after.contains("NEW-BUILD"),
        "served the old build's flags: {after}"
    );
    assert_eq!(fake.runs().len(), 2);
    assert!(
        fake.runs()[1]
            .join(" ")
            .ends_with(&format!("{ID_B} --help")),
        "{:?}",
        fake.runs()[1]
    );

    // Rolled back to the old image: its vocabulary is still filed under it.
    fake.set_image(tag, vec![image_is(ID_A, "[\"/app/entrypoint.sh\"]")]);
    let back = reg.help_text("lmgw", tag, &[]).await.unwrap();
    assert!(back.contains("OLD-BUILD"), "{back}");
    assert_eq!(fake.runs().len(), 2, "a known ID is a hit");
}

/// `forget_help` drops by ID — full or short, `sha256:` or not — because the
/// caller that needs it has just moved the tag off the image it names. Not
/// needed for correctness (the retag test above), only to let a gone image's
/// entry go.
#[tokio::test]
async fn forget_help_drops_an_image_ids_entry_and_nothing_else() {
    let fake = Arc::new(Fake::default());
    fake.set_image("localhost/a:latest", vec![image_is(ID_A, "null")]);
    fake.set_image("localhost/b:latest", vec![image_is(ID_B, "null")]);
    *fake.run_replies.lock().unwrap() = VecDeque::from(vec![
        out(help_body("A1")),
        out(help_body("B1")),
        out(help_body("A2")),
    ]);
    let reg = registry(fake.clone(), vec![]);
    reg.help_text("lmgw", "localhost/a:latest", &[])
        .await
        .unwrap();
    reg.help_text("lmgw", "localhost/b:latest", &[])
        .await
        .unwrap();

    // Too short to be an ID: a prefix of arbitrarily many, refused.
    assert_eq!(reg.forget_help("aaaa"), 0);
    assert_eq!(reg.forget_help(&format!("sha256:{}", &ID_A[..12])), 1);

    let a = reg
        .help_text("lmgw", "localhost/a:latest", &[])
        .await
        .unwrap();
    assert!(a.contains("A2"), "the forgotten image is read again: {a}");
    let b = reg
        .help_text("lmgw", "localhost/b:latest", &[])
        .await
        .unwrap();
    assert!(b.contains("B1"), "the other image kept its entry: {b}");
    assert_eq!(fake.runs().len(), 3);
}

/// ik_llama.cpp installs `/llama-server`, on no path the read used to try —
/// so every ik image failed the read, silently. The image's entrypoint names
/// the server, so it is tried first. And the read carries the caller's run
/// args: ik's binary links `libcuda.so.1` and exits 127 on `--help` without
/// the GPU device (measured against `localhost/llama-server-cuda:ik-latest`).
#[tokio::test]
async fn an_ik_image_is_read_at_its_entrypoint_with_the_gpu_attached() {
    let ik = "localhost/llama-server-cuda:ik-latest";
    let fake = Arc::new(Fake::default());
    fake.set_image(ik, vec![image_is(ID_A, "[\"/llama-server\"]")]);
    *fake.run_replies.lock().unwrap() = VecDeque::from(vec![out(help_body("IK"))]);
    let reg = registry(fake.clone(), vec![]);

    let gpu = vec!["--device".to_string(), "nvidia.com/gpu=all".to_string()];
    let text = reg.help_text("lmgw", ik, &gpu).await.unwrap();
    assert!(text.contains("IK"));
    assert_eq!(fake.entrypoints(), vec!["/llama-server"], "first try");
    let argv = &fake.runs()[0];
    let device = argv.iter().position(|a| a == "--device").expect("{argv:?}");
    let entry = argv.iter().position(|a| a == "--entrypoint").unwrap();
    assert_eq!(argv[device + 1], "nvidia.com/gpu=all");
    assert!(device < entry, "run args go before the image: {argv:?}");
}

/// A wrapper entrypoint is never taken for the server: the owner's
/// nginx-fronted images run `/app/entrypoint.sh`, which starts llama-server in
/// the background and waits — a `--help` handed to it would serve, not print.
#[tokio::test]
async fn a_wrapper_entrypoint_is_never_taken_for_the_server() {
    let fake = Arc::new(Fake::default());
    fake.set_image(
        "localhost/a:latest",
        vec![image_is(ID_A, "[\"/app/entrypoint.sh\"]")],
    );
    *fake.run_replies.lock().unwrap() = VecDeque::from(vec![
        out("crun: executable file `llama-server` not found in $PATH".into()),
        out(help_body("APP")),
    ]);
    let reg = registry(fake.clone(), vec![]);
    let text = reg
        .help_text("lmgw", "localhost/a:latest", &[])
        .await
        .unwrap();
    assert!(text.contains("APP"));
    assert_eq!(
        fake.entrypoints(),
        vec!["llama-server", "/app/llama-server"]
    );
}

/// The candidate order itself: the entrypoint only when its basename *is* the
/// server, then `PATH` and the known install paths, each once.
#[test]
fn llama_server_candidates_take_the_entrypoint_only_when_it_is_the_server() {
    let known = ["llama-server", "/app/llama-server", "/llama-server"];
    assert_eq!(llama_server_candidates(None), known);
    for not_the_server in [
        "/app/entrypoint.sh",
        "/bin/sh",
        "/app/llama-server.sh",
        "/usr/bin/llama-server-wrapper",
        "",
    ] {
        assert_eq!(
            llama_server_candidates(Some(not_the_server)),
            known,
            "{not_the_server:?}"
        );
    }
    assert_eq!(
        llama_server_candidates(Some("/llama-server")),
        ["/llama-server", "llama-server", "/app/llama-server"]
    );
    assert_eq!(
        llama_server_candidates(Some("/opt/ik/bin/llama-server")),
        [
            "/opt/ik/bin/llama-server",
            "llama-server",
            "/app/llama-server",
            "/llama-server"
        ]
    );
}

/// Every known path tried, none worked: an error naming the image, so the
/// caller can say *why* it has no vocabulary to validate against.
#[tokio::test]
async fn the_help_read_tries_every_known_path_and_then_gives_up() {
    let fake = Arc::new(Fake::default());
    fake.set_image("localhost/b:latest", vec![image_is(ID_B, "null")]);
    *fake.run_replies.lock().unwrap() = VecDeque::from(vec![
        out(String::new()),
        out(String::new()),
        out(String::new()),
    ]);
    let reg = registry(fake.clone(), vec![]);
    let err = reg
        .help_text("lmgw", "localhost/b:latest", &[])
        .await
        .unwrap_err();
    assert!(err.contains("localhost/b:latest"), "{err}");
    assert_eq!(
        fake.entrypoints(),
        vec!["llama-server", "/app/llama-server", "/llama-server"]
    );
}

/// The failure that matters is the binary that is there and broke, not the
/// "not found" of the paths tried after it — the ik image without its GPU run
/// args is exactly this.
#[tokio::test]
async fn a_failed_read_names_the_binary_that_broke() {
    let fake = Arc::new(Fake::default());
    fake.set_image(
        "localhost/ik:latest",
        vec![image_is(ID_A, "[\"/llama-server\"]")],
    );
    *fake.run_replies.lock().unwrap() = VecDeque::from(vec![
        Reply::Out(CmdOutput {
            status: 127,
            stdout: String::new(),
            stderr: "/llama-server: error while loading shared libraries: libcuda.so.1: \
                     cannot open shared object file: No such file or directory"
                .into(),
        }),
        out("crun: executable file `llama-server` not found in $PATH".into()),
        out("crun: executable file `/app/llama-server` not found".into()),
    ]);
    let reg = registry(fake.clone(), vec![]);
    let err = reg
        .help_text("lmgw", "localhost/ik:latest", &[])
        .await
        .unwrap_err();
    assert!(err.contains("localhost/ik:latest"), "{err}");
    assert!(err.contains("`/llama-server --help` exited 127"), "{err}");
    assert!(err.contains("libcuda.so.1"), "{err}");

    // All of them absent: said as such, with the entrypoint that was not one.
    let fake = Arc::new(Fake::default());
    fake.set_image(
        "localhost/w:latest",
        vec![image_is(ID_B, "[\"/start.sh\"]")],
    );
    *fake.run_replies.lock().unwrap() = ["llama-server", "/app/llama-server", "/llama-server"]
        .into_iter()
        .map(|b| out(format!("crun: executable file `{b}` not found")))
        .collect();
    let reg = registry(fake.clone(), vec![]);
    let err = reg
        .help_text("lmgw", "localhost/w:latest", &[])
        .await
        .unwrap_err();
    assert!(err.contains("none of `llama-server`"), "{err}");
    assert!(err.contains("`/start.sh`"), "{err}");
}

/// An image that is not on the box: podman's own failure (exit 125) is the
/// same for every path, so the read stops at the first and says it — and a
/// failed read is not cached.
#[tokio::test]
async fn an_image_that_is_not_on_the_box_fails_once_and_is_not_cached() {
    let image = "localhost/nope:latest";
    let fake = Arc::new(Fake::default());
    fake.set_image(image, vec![image_unknown(image)]);
    *fake.run_replies.lock().unwrap() = VecDeque::from(vec![
        Reply::Out(image_unknown(image)),
        Reply::Out(image_unknown(image)),
    ]);
    let reg = registry(fake.clone(), vec![]);
    let err = reg.help_text("lmgw", image, &[]).await.unwrap_err();
    assert!(err.contains("image not known"), "{err}");
    assert_eq!(fake.runs().len(), 1, "one podman failure is enough");
    // With no ID, the probe ran the reference.
    assert!(
        fake.runs()[0]
            .join(" ")
            .ends_with(&format!("{image} --help")),
        "{:?}",
        fake.runs()[0]
    );
    reg.help_text("lmgw", image, &[]).await.unwrap_err();
    assert_eq!(fake.runs().len(), 2, "the failure was not remembered");
}

/// An image that arrives while the read runs (a start pulled it) is filed
/// under the ID it resolves to afterwards, so the next lookup is a hit.
#[tokio::test]
async fn an_image_that_arrives_during_the_read_is_filed_under_its_id() {
    let image = "localhost/late:latest";
    let fake = Arc::new(Fake::default());
    fake.set_image(image, vec![image_unknown(image), image_is(ID_A, "null")]);
    *fake.run_replies.lock().unwrap() = VecDeque::from(vec![out(help_body("LATE"))]);
    let reg = registry(fake.clone(), vec![]);
    assert!(reg
        .help_text("lmgw", image, &[])
        .await
        .unwrap()
        .contains("LATE"));
    assert!(reg
        .help_text("lmgw", image, &[])
        .await
        .unwrap()
        .contains("LATE"));
    assert_eq!(fake.runs().len(), 1, "{:?}", fake.verbs());
}

/// The sd-server vocabulary is keyed the same way: a retagged image is probed
/// again, under its new ID.
#[tokio::test]
async fn a_retagged_sd_image_is_probed_again() {
    let tag = "localhost/sd-server-cuda:latest";
    let fake = Arc::new(Fake::default());
    *fake.help.lock().unwrap() = Some(SD_HELP.to_string());
    fake.set_image(tag, vec![image_is(ID_A, "[\"/sd-cli\"]")]);
    let reg = registry(fake.clone(), vec![]);

    reg.sdcpp_caps("lmgw", tag, &[]).await.unwrap();
    reg.sdcpp_caps("lmgw", tag, &[]).await.unwrap();
    assert_eq!(fake.help_runs().len(), 1);
    // The server is named explicitly, never taken from the entrypoint: a
    // start always runs `--entrypoint /sd-server`.
    assert!(
        fake.help_runs()[0]
            .join(" ")
            .ends_with(&format!("{ID_A} --help")),
        "{:?}",
        fake.help_runs()[0]
    );

    fake.set_image(tag, vec![image_is(ID_B, "[\"/sd-cli\"]")]);
    reg.sdcpp_caps("lmgw", tag, &[]).await.unwrap();
    assert_eq!(fake.help_runs().len(), 2, "a new ID is a new vocabulary");
    assert_eq!(
        reg.forget_help(ID_B),
        1,
        "sd-server entries are forgotten too"
    );
}

// ---------------------------------------------------------------------------
// The image class (image-generation design §3)
// ---------------------------------------------------------------------------

/// The committed `sd-server --help` of the spiked image, the same bytes the
/// renderer falls back to — here it stands in for what a real throwaway
/// container would print.
const SD_HELP: &str = include_str!("../fixtures/sdcpp/sd-server-help-c678dfe.txt");
/// A real `GET /sdcpp/v1/capabilities` body (Z-Image-Turbo, §12.3).
const SD_CAPS: &str = include_str!("../fixtures/sdcpp/capabilities-z-image-turbo-c678dfe.json");

fn image_runtime(model_id: &str, modes: &[&str], edit: bool) -> ModelRuntime {
    ModelRuntime {
        class: Class::Image,
        model_id: model_id.into(),
        image: "ghcr.io/leejet/stable-diffusion.cpp:master-cuda".into(),
        extra_run_args: vec!["--device".into(), "nvidia.com/gpu=all".into()],
        idle_seconds: 600,
        warm_start: false,
        enabled: true,
        llama: None,
        audio: None,
        audio_settings: None,
        image_model: Some(ImageModel {
            id: 1,
            model_id: model_id.into(),
            files: serde_json::json!({"diffusion_model": "z/z_image_turbo-Q4_K.gguf"})
                .as_object()
                .cloned()
                .unwrap(),
            args: serde_json::json!({"diffusion_fa": true})
                .as_object()
                .cloned()
                .unwrap(),
            modes: modes.iter().map(|s| s.to_string()).collect(),
            edit,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            idle_seconds: 600,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            peak_extra_bytes: None,
            peak_learned_at: None,
        }),
        sdcpp_caps: None,
        rung: None,
    }
}

/// An sd-server mock: `GET /sdcpp/v1/capabilities` with the status (and body)
/// the test wants. A 500 additionally carries the `EXCEPTION_WHAT` header the
/// real server sets, which is the only description of the failure that exists.
async fn sd_server(status: u16) -> MockServer {
    let server = MockServer::start().await;
    let template = if status == 200 {
        ResponseTemplate::new(200).set_body_string(SD_CAPS)
    } else {
        ResponseTemplate::new(status)
            .insert_header(
                "EXCEPTION_WHAT",
                "filesystem error: status: Operation not permitted [./proc/1/map_files/]",
            )
            .set_body_string(r#"{"error":"server_error"}"#)
    };
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(template)
        .mount(&server)
        .await;
    server
}

/// An image model's own `models_dir`, because `render_spec` creates the two
/// directories the argv points at before it returns.
fn image_spec<'a>(rt: &'a ModelRuntime, models_dir: &'a str, load_ms: u64) -> AcquireSpec<'a> {
    AcquireSpec {
        runtime: rt,
        container_prefix: "lmgw",
        models_dir,
        data_dir: Path::new("/nonexistent/lmgw-test-data-dir"),
        may_write_models_dir: true,
        load_timeout: Duration::from_millis(load_ms),
        stop_timeout: Duration::from_millis(500),
    }
}

/// The fourth class, end to end: the vocabulary probe runs with the class's
/// device flags and `--entrypoint /sd-server` (§12.7 — `-h` needs the GPU),
/// the container starts with `--init` and the image's entrypoint overridden,
/// and the capabilities the 200 carried are kept on the entry for WP2.
#[tokio::test]
async fn an_image_start_probes_the_vocabulary_and_keeps_the_capabilities() {
    let sd = sd_server(200).await;
    let models = tempfile::tempdir().unwrap();
    let dir = models.path().display().to_string();
    let fake = Arc::new(Fake::default());
    *fake.help.lock().unwrap() = Some(SD_HELP.to_string());
    // Two ports: the model is started, stopped and started again, to prove
    // the vocabulary is read once rather than per start.
    let reg = registry(fake.clone(), vec![sd.address().port(), sd.address().port()]);

    let rt = image_runtime("z-image-turbo", &["img_gen"], false);
    let guard = reg.acquire(&image_spec(&rt, &dir, 5_000)).await.unwrap();
    assert_eq!(guard.class(), Class::Image);

    // One help probe, with the entrypoint and the device flags it needs.
    let help = fake.help_runs();
    assert_eq!(help.len(), 1, "{help:?}");
    assert!(
        help[0]
            .windows(2)
            .any(|w| w[0] == "--entrypoint" && w[1] == "/sd-server"),
        "{:?}",
        help[0]
    );
    assert!(
        help[0]
            .windows(2)
            .any(|w| w[0] == "--device" && w[1] == "nvidia.com/gpu=all"),
        "the probe needs the GPU or the binary exits 127: {:?}",
        help[0]
    );

    let runs = fake.runs();
    assert_eq!(runs.len(), 1);
    let argv = &runs[0];
    assert!(argv.contains(&"--init".to_string()), "{argv:?}");
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--entrypoint" && w[1] == "/sd-server"),
        "{argv:?}"
    );
    assert!(
        argv.contains(&container_name("lmgw", Class::Image, "z-image-turbo")),
        "{argv:?}"
    );
    assert!(argv.contains(&"lmgw.engine=sdcpp".to_string()), "{argv:?}");
    assert!(argv.contains(&"lmgw.class=image".to_string()), "{argv:?}");

    let view = reg.list();
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].state, RuntimeState::Ready);
    assert!(view[0].warnings.is_empty(), "{:?}", view[0].warnings);
    let caps = view[0]
        .image_capabilities
        .as_ref()
        .expect("the capabilities body is kept on the entry");
    assert_eq!(caps.supported_modes, vec!["img_gen"]);
    assert_eq!(caps.samplers.len(), 21);
    assert_eq!(caps.limits.max_width, Some(4096));

    // The vocabulary is cached per image: a second start does not re-probe.
    drop(guard);
    reg.stop(Class::Image, "z-image-turbo", true).await.unwrap();
    reg.acquire(&image_spec(&rt, &dir, 5_000))
        .await
        .expect("the second start works the same way");
    assert_eq!(fake.runs().len(), 2);
    assert_eq!(
        fake.help_runs().len(),
        1,
        "the help text is cached by image"
    );
}

/// §12.2, the measured one: the capabilities route throws a filesystem
/// exception when its scan trips, while generation works perfectly. That is
/// **ready with a warning**, not a failed start — and it must not burn the
/// load timeout waiting for a 200 that will never come.
#[tokio::test]
async fn a_5xx_from_the_capabilities_route_is_ready_with_a_warning() {
    let sd = sd_server(500).await;
    let models = tempfile::tempdir().unwrap();
    let dir = models.path().display().to_string();
    let fake = Arc::new(Fake::default());
    *fake.help.lock().unwrap() = Some(SD_HELP.to_string());
    let reg = registry(fake.clone(), vec![sd.address().port()]);

    let rt = image_runtime("z-image-turbo", &["img_gen"], false);
    let started = std::time::Instant::now();
    let guard = reg
        .acquire(&image_spec(&rt, &dir, 10_000))
        .await
        .expect("a 5xx capabilities route is not a failed start");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "ready-with-warning burned the load timeout: {elapsed:?}"
    );
    assert_eq!(guard.class(), Class::Image);

    let view = reg.list();
    assert_eq!(view[0].state, RuntimeState::Ready);
    let warnings = &view[0].warnings;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("filesystem error"), "{warnings:?}");
    assert!(warnings[0].contains("500"), "{warnings:?}");
    assert!(
        view[0].image_capabilities.is_none(),
        "nothing was published: the body was an error"
    );
}

/// A vocabulary read that fails (image not pulled, GPU taken — the probe
/// needs the device) must never block a start: the argv is spelled against
/// the build lmgw ships with, and the entry says so.
#[tokio::test]
async fn a_failed_help_probe_falls_back_to_the_embedded_vocabulary() {
    let sd = sd_server(200).await;
    let models = tempfile::tempdir().unwrap();
    let dir = models.path().display().to_string();
    let fake = Arc::new(Fake::default());
    // `help` left at None: the throwaway exits 127, as it does without the
    // GPU device attached.
    let reg = registry(fake.clone(), vec![sd.address().port()]);

    let rt = image_runtime("z-image-turbo", &["img_gen"], false);
    reg.acquire(&image_spec(&rt, &dir, 5_000))
        .await
        .expect("a start never waits on a vocabulary read");

    let runs = fake.runs();
    assert_eq!(runs.len(), 1, "the container still started: {runs:?}");
    assert!(
        runs[0]
            .windows(2)
            .any(|w| w[0] == "--diffusion-model" && w[1] == "/models/z/z_image_turbo-Q4_K.gguf"),
        "{:?}",
        runs[0]
    );
    let view = reg.list();
    let warnings = &view[0].warnings;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("vocabulary"), "{warnings:?}");
    assert!(warnings[0].contains("ships with"), "{warnings:?}");
    // A failed probe is not cached: the next start asks again.
    assert_eq!(fake.help_runs().len(), 1);
}

/// "Operator says, probe warns" (§4): the row's `modes`/`edit` are what
/// exposure is built from, and a pipeline that does not back them is a
/// warning on the entry, never a silent correction.
#[tokio::test]
async fn a_row_claiming_more_than_the_pipeline_supports_starts_with_a_warning() {
    let sd = sd_server(200).await;
    let models = tempfile::tempdir().unwrap();
    let dir = models.path().display().to_string();
    let fake = Arc::new(Fake::default());
    *fake.help.lock().unwrap() = Some(SD_HELP.to_string());
    let reg = registry(fake.clone(), vec![sd.address().port()]);

    let rt = image_runtime("z-image-turbo", &["img_gen", "vid_gen"], false);
    reg.acquire(&image_spec(&rt, &dir, 5_000)).await.unwrap();

    let view = reg.list();
    assert_eq!(view[0].state, RuntimeState::Ready);
    let warnings = &view[0].warnings;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("vid_gen"), "{warnings:?}");
    assert!(
        view[0].image_capabilities.is_some(),
        "the probe still published what it read"
    );
}

/// A row that names neither `model` nor `diffusion_model` cannot produce an
/// argv, and the refusal has to arrive *before* `podman run` — the container
/// would otherwise exit with a usage dump that names nothing.
#[tokio::test]
async fn an_unrenderable_image_row_fails_before_podman_is_called() {
    let sd = sd_server(200).await;
    let models = tempfile::tempdir().unwrap();
    let dir = models.path().display().to_string();
    let fake = Arc::new(Fake::default());
    *fake.help.lock().unwrap() = Some(SD_HELP.to_string());
    let reg = registry(fake.clone(), vec![sd.address().port()]);

    let mut rt = image_runtime("broken", &["img_gen"], false);
    rt.image_model.as_mut().unwrap().files.clear();
    let err = reg
        .acquire(&image_spec(&rt, &dir, 5_000))
        .await
        .expect_err("a row with no pipeline to load cannot start");
    assert!(err.to_string().contains("neither"), "{err}");
    assert!(fake.runs().is_empty(), "{:?}", fake.runs());
    assert!(reg.list().is_empty(), "the claim was released");
}

/// A class's run args as a throwaway probe takes them: what contradicts a
/// foreground `--rm --replace`d container is dropped, the rest kept.
#[test]
fn a_throwaway_takes_the_run_args_minus_what_contradicts_it() {
    use lmgw_core::runtime::registry::{throwaway_args, without_gpus};
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        throwaway_args(&v(&[
            "--device",
            "nvidia.com/gpu=all",
            "-d",
            "--restart",
            "always",
            "--restart=on-failure",
            "-p",
            "8080:80",
            "-p9090:90",
            "--publish=1:1",
            "--publish",
            "2:2",
            "-P",
            "--publish-all",
            "--name",
            "x",
            "--name=y",
            "--detach=true",
            "--security-opt",
            "label=disable",
        ])),
        v(&[
            "--device",
            "nvidia.com/gpu=all",
            "--security-opt",
            "label=disable"
        ])
    );
    assert_eq!(
        without_gpus(&v(&["--device", "nvidia.com/gpu=all"])),
        v(&[
            "--device",
            "nvidia.com/gpu=all",
            "-e",
            "CUDA_VISIBLE_DEVICES="
        ])
    );
}
