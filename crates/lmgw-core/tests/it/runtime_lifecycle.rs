//! Boot reconciliation, the legacy sweep, warm starts, the idle reaper,
//! graceful shutdown and delete/disable hygiene (design §3.4, §3.6, §3.7).
//!
//! The fake podman here is a **stateful** one, unlike `runtime_registry.rs`'s
//! call recorder: it owns a list of containers that `run` adds to, `stop`
//! flips to `exited`, `rm` deletes, and — the point of this file — `ps` and
//! `inspect` report on. `ps` applies the `--filter label=…` it is handed
//! exactly as podman would, which is what makes "reconciliation matches on the
//! instance label, never on a name pattern" (§3.3) an assertable property
//! rather than a comment: a container named as if it were ours but carrying no
//! labels is invisible to the filter and therefore survives.
//!
//! Readiness is real HTTP throughout — each container's published port is a
//! wiremock — so an adoption that says "health-probed" has actually probed
//! something, and one pointed at a port answering 500 fails for the reason it
//! claims.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::config::{AudioModel, AudioSettings, LlamaParams, Settings};
use lmgw_core::ops::{self, LocalModelPatch};
use lmgw_core::runtime::argv::{render_engine_args, LlamaArgs};
use lmgw_core::runtime::descriptor::{model_runtime, ModelRuntime};
use lmgw_core::runtime::lifecycle;
use lmgw_core::runtime::registry::{AcquireSpec, CmdOutput, CommandRunner, Registry, RuntimeState};
use lmgw_core::runtime::{container_name, Class};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewLocalModel};
use serde_json::json;
use tokio::sync::watch;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;

// ---------------------------------------------------------------------------
// The fake podman
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct FakeContainer {
    name: String,
    labels: HashMap<String, String>,
    /// `running` | `exited` | `created`, exactly the strings podman prints.
    state: String,
    /// `Config.Cmd`: everything after the image name.
    cmd: Vec<String>,
    host_port: u16,
}

#[derive(Default)]
struct Podman {
    calls: Mutex<Vec<Vec<String>>>,
    containers: Mutex<Vec<FakeContainer>>,
    /// When set, every `podman inspect` parks here *after* reading the
    /// container list — the seam that lets a test hold an adoption open and
    /// prove what happens when an `acquire` claims the same model meanwhile.
    inspect_gate: Mutex<Option<watch::Receiver<bool>>>,
    /// When set, `podman inspect` fails with this stderr instead of answering
    /// — a podman that is *there* but cannot say anything (socket gone,
    /// storage locked), as opposed to "no such container".
    inspect_broken: Mutex<Option<String>>,
}

fn ok(stdout: &str) -> CmdOutput {
    CmdOutput {
        status: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

fn no_such(name: &str) -> CmdOutput {
    CmdOutput {
        status: 125,
        stdout: String::new(),
        stderr: format!("Error: no such container {name}"),
    }
}

/// `--label k=v` / `--filter label=k=v` style lookups over an argv.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

#[async_trait::async_trait]
impl CommandRunner for Podman {
    async fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        assert_eq!(program, "podman", "the registry only ever shells podman");
        self.calls.lock().unwrap().push(args.to_vec());
        match args[0].as_str() {
            // Applies the label filter for real: that is what proves
            // reconciliation never falls back to matching names.
            "ps" => {
                let filter = flag_value(args, "--filter")
                    .and_then(|f| f.strip_prefix("label="))
                    .map(|kv| {
                        let (k, v) = kv.split_once('=').expect("label filter is key=value");
                        (k.to_string(), v.to_string())
                    });
                let rows: Vec<serde_json::Value> = self
                    .containers
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|c| match &filter {
                        Some((k, v)) => c.labels.get(k).is_some_and(|got| got == v),
                        None => true,
                    })
                    .map(|c| {
                        json!({
                            "Names": [c.name],
                            "Labels": c.labels,
                            "State": c.state,
                            // Fields reconciliation must not read, present so a
                            // test would notice if it started to.
                            "Id": "deadbeef",
                            "Image": "some/image:tag",
                        })
                    })
                    .collect();
                Ok(ok(&serde_json::to_string(&rows).unwrap()))
            }
            "inspect" => {
                let name = args.last().cloned().unwrap_or_default();
                if let Some(stderr) = self.inspect_broken.lock().unwrap().clone() {
                    return Ok(CmdOutput {
                        status: 125,
                        stdout: String::new(),
                        stderr,
                    });
                }
                let found = self
                    .containers
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|c| c.name == name)
                    .cloned();
                // Read the row first, then park: an adoption that resumes here
                // is deciding on the container it saw, which is exactly the
                // stale view the identity-checked insert has to catch.
                let gate = self.inspect_gate.lock().unwrap().clone();
                if let Some(mut rx) = gate {
                    while !*rx.borrow_and_update() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                }
                match found {
                    None => Ok(no_such(&name)),
                    Some(c) => Ok(ok(&serde_json::to_string(&json!([{
                        "Config": {"Cmd": c.cmd},
                        "NetworkSettings": {"Ports": {
                            "8080/tcp": [{"HostIp": "0.0.0.0", "HostPort": c.host_port.to_string()}]
                        }},
                    }]))
                    .unwrap())),
                }
            }
            // A throwaway probe container (§3.6's probe/help: `run --rm`,
            // foreground, no published port) is not a managed container and
            // must not land in the map. Answering with empty output is what a
            // box with no image pulled would do, and the caller degrades to
            // "no vocabulary to check against" exactly as it does in
            // production.
            "run" if args.iter().any(|a| a == "--rm") => Ok(ok("")),
            "run" => {
                let name = flag_value(args, "--name").unwrap_or_default().to_string();
                let host_port: u16 = flag_value(args, "-p")
                    .and_then(|p| p.rsplit(':').nth(1))
                    .and_then(|p| p.parse().ok())
                    .expect("every managed container publishes a host port");
                let labels: HashMap<String, String> = args
                    .iter()
                    .zip(args.iter().skip(1))
                    .filter(|(f, _)| *f == "--label")
                    .filter_map(|(_, kv)| kv.split_once('='))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                // The image is the token right after the last `:ro` mount; the
                // container command is everything after it.
                let image_at = args
                    .iter()
                    .rposition(|a| a.ends_with(":ro"))
                    .expect("every managed container mounts /models")
                    + 1;
                let mut list = self.containers.lock().unwrap();
                list.retain(|c| c.name != name); // --replace
                list.push(FakeContainer {
                    name,
                    labels,
                    state: "running".into(),
                    cmd: args[image_at + 1..].to_vec(),
                    host_port,
                });
                Ok(ok("c0ffee\n"))
            }
            "stop" => {
                let name = args.last().cloned().unwrap_or_default();
                let mut list = self.containers.lock().unwrap();
                match list.iter_mut().find(|c| c.name == name) {
                    Some(c) => {
                        c.state = "exited".into();
                        Ok(ok(""))
                    }
                    None => Ok(no_such(&name)),
                }
            }
            "rm" => {
                let name = args.last().cloned().unwrap_or_default();
                let mut list = self.containers.lock().unwrap();
                let before = list.len();
                list.retain(|c| c.name != name);
                if list.len() == before {
                    Ok(no_such(&name))
                } else {
                    Ok(ok(""))
                }
            }
            _ => Ok(ok("")),
        }
    }
}

impl Podman {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    /// Every call to one podman verb, in order.
    fn verb(&self, verb: &str) -> Vec<Vec<String>> {
        self.calls().into_iter().filter(|c| c[0] == verb).collect()
    }

    /// Names passed to `podman rm`, in order.
    fn removed(&self) -> Vec<String> {
        self.verb("rm")
            .into_iter()
            .map(|c| c.last().cloned().unwrap_or_default())
            .collect()
    }

    /// Names passed to `podman stop`, in order.
    fn stopped(&self) -> Vec<String> {
        self.verb("stop")
            .into_iter()
            .map(|c| c.last().cloned().unwrap_or_default())
            .collect()
    }

    fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self
            .containers
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        n.sort();
        n
    }

    fn add(&self, c: FakeContainer) {
        self.containers.lock().unwrap().push(c);
    }
}

// ---------------------------------------------------------------------------
// Container mocks
// ---------------------------------------------------------------------------

/// A container that answers `/health`, audio's `/v1/models` and llama's `/props` with `status`.
async fn container_at(status: u16) -> MockServer {
    let server = MockServer::start().await;
    for p in ["/health", "/v1/models", "/props"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(status).set_body_string(r#"{"status":"ok"}"#))
            .mount(&server)
            .await;
    }
    server
}

// ---------------------------------------------------------------------------
// Reconciliation, driven directly
// ---------------------------------------------------------------------------

fn chat_runtime(model_id: &str) -> ModelRuntime {
    ModelRuntime {
        class: Class::Chat,
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

const DATA_DIR: &str = "/nonexistent/lmgw-test-data-dir";

/// A real `GET /sdcpp/v1/capabilities` body (Z-Image-Turbo, §12.3) — for this
/// class the readiness probe and the capabilities probe are one request.
const SD_CAPS: &str = include_str!("../fixtures/sdcpp/capabilities-z-image-turbo-c678dfe.json");

/// One sd-server pipeline as the registry sees it. `sdcpp_caps: None` is a box
/// that cannot run the throwaway `--help` container, which is what the fake
/// podman above answers — the argv is then spelled against the embedded
/// vocabulary, on both sides of the comparison.
fn image_runtime(model_id: &str) -> ModelRuntime {
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
        image_model: Some(lmgw_core::config::ImageModel {
            id: 1,
            model_id: model_id.into(),
            files: json!({"diffusion_model": "z/z_image_turbo-Q4_K.gguf"})
                .as_object()
                .cloned()
                .unwrap(),
            args: json!({"diffusion_fa": true}).as_object().cloned().unwrap(),
            modes: vec!["img_gen".into()],
            edit: false,
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

fn spec(rt: &ModelRuntime) -> AcquireSpec<'_> {
    AcquireSpec {
        runtime: rt,
        container_prefix: "lmgw",
        models_dir: "/srv/models",
        data_dir: Path::new(DATA_DIR),
        may_write_models_dir: true,
        load_timeout: Duration::from_millis(2_000),
        stop_timeout: Duration::from_millis(500),
    }
}

/// The container command lmgw would render for this model — the thing
/// adoption compares `Config.Cmd` against.
fn rendered_cmd(rt: &ModelRuntime, port: u16) -> Vec<String> {
    let render = rt
        .render_spec("lmgw", port, "/srv/models", Path::new(DATA_DIR))
        .unwrap();
    render_engine_args(&render)
}

/// The labels [`podman_run_argv`](lmgw_core::runtime::argv::podman_run_argv)
/// puts on every managed container (§3.3), its run args' digest for the
/// args this file's runtimes of `class` carry.
fn managed(class: Class, model_id: &str) -> HashMap<String, String> {
    let args: Vec<String> = match class {
        Class::Audio => vec![],
        _ => vec!["--device".into(), "nvidia.com/gpu=all".into()],
    };
    managed_with(class, model_id, &args)
}

/// [`managed`] for a row of `f`'s, with the run args its descriptor
/// renders now.
fn managed_now(f: &Fixture, class: Class, model_id: &str) -> HashMap<String, String> {
    let snap = f.state.snapshot();
    let rt = model_runtime(&snap, class, model_id).expect("the row");
    managed_with(class, model_id, &rt.extra_run_args)
}

/// [`managed`], started with `run_args`.
fn managed_with(class: Class, model_id: &str, run_args: &[String]) -> HashMap<String, String> {
    HashMap::from([
        ("lmgw.instance".to_string(), "lmgw".to_string()),
        ("lmgw.class".to_string(), class.as_str().to_string()),
        ("lmgw.model".to_string(), model_id.to_string()),
        ("lmgw.engine".to_string(), class.engine().to_string()),
        (
            lmgw_core::runtime::argv::RUN_ARGS_LABEL.to_string(),
            lmgw_core::runtime::argv::run_args_digest(run_args),
        ),
    ])
}

fn registry(podman: Arc<Podman>, ports: Vec<u16>) -> Arc<Registry> {
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(ports)));
    Arc::new(Registry::with_ports(
        podman,
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

#[tokio::test]
async fn reconcile_adopts_a_matching_running_container_and_acquire_reuses_it() {
    let health = container_at(200).await;
    let port = health.address().port();
    let rt = chat_runtime("m1");
    let name = container_name("lmgw", Class::Chat, "m1");

    let podman = Arc::new(Podman::default());
    podman.add(FakeContainer {
        name: name.clone(),
        labels: managed(Class::Chat, "m1"),
        state: "running".into(),
        cmd: rendered_cmd(&rt, port),
        host_port: port,
    });
    // No ports to hand out: a start would fail loudly, which is exactly the
    // assertion "adoption means no new container" wants.
    let reg = registry(podman.clone(), vec![]);

    let report = reg.reconcile("lmgw", &[spec(&rt)]).await;
    assert_eq!(report.adopted, vec![name.clone()]);
    assert!(report.removed.is_empty(), "{:?}", report.removed);
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    // The port came back from `podman inspect`, not from an allocator.
    let live = reg.list();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].port, port);
    assert_eq!(live[0].state, RuntimeState::Ready);
    assert_eq!(live[0].container_name, name);

    let guard = reg
        .acquire(&spec(&rt))
        .await
        .expect("adopted entry is live");
    assert_eq!(guard.port(), port);
    assert!(
        podman.verb("run").is_empty(),
        "an adopted container must not be started again: {:?}",
        podman.calls()
    );
    // The request gate's facts ride on the adopted entry too (second review,
    // finding 1): the row the container's argv was just matched against.
    let facts = guard
        .gate_facts()
        .expect("a chat entry carries its start facts");
    assert_eq!(facts.model_id, "m1");
    assert_eq!(facts.models_dir, "/srv/models");
    assert_eq!(facts.gguf_path, "m1.gguf");
}

/// A container whose `podman run` flags are not the ones its row renders
/// now is not adopted: one started with an empty override (no GPU, no
/// `label=disable`) before migration 0055 moved the row onto its class's
/// flags, and one from an lmgw that kept no record of its flags. Both are
/// removed, saying why, and the next start renders the flags in effect.
#[tokio::test]
async fn reconcile_removes_a_container_whose_run_args_differ_from_the_render() {
    let health = container_at(200).await;
    let port = health.address().port();
    let rt = chat_runtime("m1");
    let name = container_name("lmgw", Class::Chat, "m1");
    for (labels, why) in [
        (
            managed_with(Class::Chat, "m1", &[]),
            "its run args are not the ones",
        ),
        (
            {
                let mut l = managed(Class::Chat, "m1");
                l.remove(lmgw_core::runtime::argv::RUN_ARGS_LABEL);
                l
            },
            "no record of its run args",
        ),
    ] {
        let podman = Arc::new(Podman::default());
        podman.add(FakeContainer {
            name: name.clone(),
            labels,
            state: "running".into(),
            cmd: rendered_cmd(&rt, port),
            host_port: port,
        });
        let reg = registry(podman.clone(), vec![]);
        let report = reg.reconcile("lmgw", &[spec(&rt)]).await;
        assert!(report.adopted.is_empty(), "{why}: {:?}", report.adopted);
        assert_eq!(report.removed.len(), 1, "{why}");
        assert!(report.removed[0].1.contains(why), "{:?}", report.removed);
        assert!(reg.list().is_empty());
    }
}

/// Reconciliation runs concurrently with the listener on purpose (`server.rs`
/// spawns `boot` as a task — a gateway that refused traffic until podman had
/// answered would be the worse failure), so an `acquire` can land on the same
/// model while an adoption is mid-flight. Between adoption's "is this key
/// free?" and its insert there are three awaits: `podman inspect`, the config
/// read and a `/health` probe.
///
/// The insert therefore re-asks under the lock it inserts in. Overwriting
/// would replace a **live** entry — a container that was just started, with a
/// claim already counted against it — with a stale row pointing at the port
/// the old container used, stranding the running one with nothing in the map
/// that names it. And it must not remove anything either: losing the race is
/// not a rejection of the container.
#[tokio::test]
async fn an_adoption_that_loses_the_race_yields_and_removes_nothing() {
    let old = container_at(200).await;
    let fresh = container_at(200).await;
    let old_port = old.address().port();
    let fresh_port = fresh.address().port();
    assert_ne!(old_port, fresh_port);

    let rt = chat_runtime("m1");
    let name = container_name("lmgw", Class::Chat, "m1");
    let podman = Arc::new(Podman::default());
    podman.add(FakeContainer {
        name: name.clone(),
        labels: managed(Class::Chat, "m1"),
        state: "running".into(),
        cmd: rendered_cmd(&rt, old_port),
        host_port: old_port,
    });
    let (open, gate) = watch::channel(false);
    *podman.inspect_gate.lock().unwrap() = Some(gate);
    let reg = registry(podman.clone(), vec![fresh_port]);

    // The adoption parks inside `podman inspect`, holding the pre-acquire view
    // of the container it is about to adopt.
    let reconciling = tokio::spawn({
        let reg = reg.clone();
        let rt = rt.clone();
        async move { reg.reconcile("lmgw", &[spec(&rt)]).await }
    });
    for _ in 0..3_000 {
        if podman.verb("inspect").len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(podman.verb("inspect").len(), 1, "the adoption never parked");

    // …and a request claims the model in that window: `podman run --replace`
    // takes the name over, on a port of its own.
    let guard = reg.acquire(&spec(&rt)).await.expect("the live start");
    assert_eq!(guard.port(), fresh_port);

    open.send_replace(true);
    let report = reconciling.await.unwrap();

    let live = reg.list();
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(
        live[0].port, fresh_port,
        "the adoption overwrote the live entry with its stale view"
    );
    assert_eq!(live[0].in_flight, 1, "…and with it, a live request's claim");
    assert!(
        podman.removed().is_empty(),
        "yielding is not rejecting — nothing may be removed: {:?}",
        podman.removed()
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    // Still reported as adopted: the name is spoken for, which is what the
    // legacy sweep's guard needs to know.
    assert_eq!(report.adopted, vec![name]);
    drop(guard);
}

#[tokio::test]
async fn reconcile_removes_everything_it_cannot_vouch_for() {
    let healthy = container_at(200).await;
    let sick = container_at(500).await;
    let good_port = healthy.address().port();
    let sick_port = sick.address().port();

    let keep = chat_runtime("keep");
    let stale = chat_runtime("stale");
    let exited = chat_runtime("exited");
    let unhealthy = chat_runtime("unhealthy");

    let names: HashMap<&str, String> = ["keep", "stale", "exited", "unhealthy", "gone"]
        .into_iter()
        .map(|m| (m, container_name("lmgw", Class::Chat, m)))
        .collect();

    let podman = Arc::new(Podman::default());
    podman.add(FakeContainer {
        name: names["keep"].clone(),
        labels: managed(Class::Chat, "keep"),
        state: "running".into(),
        cmd: rendered_cmd(&keep, good_port),
        host_port: good_port,
    });
    podman.add(FakeContainer {
        name: names["stale"].clone(),
        labels: managed(Class::Chat, "stale"),
        state: "running".into(),
        // Started before somebody changed the model's flags.
        cmd: vec![
            "-m".into(),
            "/models/stale.gguf".into(),
            "--ctx-size".into(),
            "4096".into(),
        ],
        host_port: good_port,
    });
    podman.add(FakeContainer {
        name: names["exited"].clone(),
        labels: managed(Class::Chat, "exited"),
        state: "exited".into(),
        cmd: rendered_cmd(&exited, good_port),
        host_port: good_port,
    });
    podman.add(FakeContainer {
        name: names["unhealthy"].clone(),
        labels: managed(Class::Chat, "unhealthy"),
        state: "running".into(),
        cmd: rendered_cmd(&unhealthy, sick_port),
        host_port: sick_port,
    });
    // A model nobody has a row for any more (deleted, or disabled — a disabled
    // model is never a reconciliation candidate).
    podman.add(FakeContainer {
        name: names["gone"].clone(),
        labels: managed(Class::Chat, "gone"),
        state: "running".into(),
        cmd: rendered_cmd(&chat_runtime("gone"), good_port),
        host_port: good_port,
    });
    // Named exactly as if it were ours, but carrying no labels: reconciliation
    // filters on `lmgw.instance` and must never see it (§3.3).
    podman.add(FakeContainer {
        name: "lmgw-chat-impostor-abcdef".into(),
        labels: HashMap::new(),
        state: "running".into(),
        cmd: vec![],
        host_port: good_port,
    });

    let reg = registry(podman.clone(), vec![]);
    let specs = [spec(&keep), spec(&stale), spec(&exited), spec(&unhealthy)];
    let report = reg.reconcile("lmgw", &specs).await;

    assert_eq!(report.adopted, vec![names["keep"].clone()]);
    let mut removed: Vec<String> = report.removed.iter().map(|(n, _)| n.clone()).collect();
    removed.sort();
    let mut expected = vec![
        names["stale"].clone(),
        names["exited"].clone(),
        names["unhealthy"].clone(),
        names["gone"].clone(),
    ];
    expected.sort();
    assert_eq!(removed, expected, "reasons: {:?}", report.removed);

    // Each rejection says why, in the words the log line uses.
    let reason = |m: &str| {
        report
            .removed
            .iter()
            .find(|(n, _)| *n == names[m])
            .map(|(_, r)| r.clone())
            .unwrap()
    };
    assert!(reason("stale").contains("command"), "{}", reason("stale"));
    assert!(
        reason("exited").contains("not running"),
        "{}",
        reason("exited")
    );
    assert!(
        reason("unhealthy").contains("no HTTP 200"),
        "{}",
        reason("unhealthy")
    );
    assert!(reason("gone").contains("configured"), "{}", reason("gone"));

    let mut survivors = vec![
        names["keep"].clone(),
        "lmgw-chat-impostor-abcdef".to_string(),
    ];
    survivors.sort();
    assert_eq!(
        podman.names(),
        survivors,
        "the unlabelled look-alike must survive: the label filter is the only matcher"
    );
}

/// An audio runtime; its per-model `server.json` lands under whichever
/// `data_dir` the spec carries.
fn audio_runtime(model_id: &str, family: &str) -> ModelRuntime {
    ModelRuntime {
        class: Class::Audio,
        model_id: model_id.into(),
        image: "ghcr.io/0xshug0/audio.cpp:full-cuda12".into(),
        extra_run_args: vec![],
        idle_seconds: 0,
        warm_start: false,
        enabled: true,
        llama: None,
        audio: Some(AudioModel {
            id: 1,
            model_id: model_id.into(),
            family: family.into(),
            path: "voices/qwen".into(),
            task: "tts".into(),
            mode: "offline".into(),
            lazy: None,
            busy_timeout_ms: None,
            backend: None,
            threads: None,
            load_options: Default::default(),
            session_options: Default::default(),
            default_request_options: Default::default(),
            model_spec_override: None,
            config_id: None,
            weight_id: None,
            voice_presets: Default::default(),
            default_voice_preset: None,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            residency: None,
        }),
        audio_settings: Some(AudioSettings::default()),
        image_model: None,
        sdcpp_caps: None,
        rung: None,
    }
}

/// An image container that was already running when lmgw started publishes
/// its samplers, limits and supported modes **from the adoption probe**.
///
/// For this class the readiness route *is* `GET /sdcpp/v1/capabilities` (§3),
/// so the one request adoption already makes carries the document. Discarding
/// it left a warm-started container advertising nothing until somebody
/// restarted it — the exact case adoption exists to avoid.
#[tokio::test]
async fn an_adopted_image_container_keeps_the_capabilities_its_probe_read() {
    let sd = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sdcpp/v1/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SD_CAPS))
        .mount(&sd)
        .await;
    let port = sd.address().port();
    let models = tempfile::tempdir().unwrap();
    let dir = models.path().display().to_string();

    let rt = image_runtime("z-image-turbo");
    let spec = AcquireSpec {
        runtime: &rt,
        container_prefix: "lmgw",
        models_dir: &dir,
        data_dir: Path::new(DATA_DIR),
        may_write_models_dir: true,
        load_timeout: Duration::from_millis(2_000),
        stop_timeout: Duration::from_millis(500),
    };
    let render = rt
        .preview_spec("lmgw", port, &dir, Path::new(DATA_DIR))
        .unwrap();
    let name = container_name("lmgw", Class::Image, "z-image-turbo");
    let podman = Arc::new(Podman::default());
    podman.add(FakeContainer {
        name: name.clone(),
        labels: managed(Class::Image, "z-image-turbo"),
        state: "running".into(),
        cmd: render_engine_args(&render),
        host_port: port,
    });
    // No ports to hand out: adoption must not start anything.
    let reg = registry(podman.clone(), vec![]);

    let report = reg.reconcile("lmgw", &[spec]).await;
    assert_eq!(report.adopted, vec![name], "{:?}", report.removed);
    let view = reg.list();
    assert_eq!(view.len(), 1);
    let caps = view[0]
        .image_capabilities
        .as_ref()
        .expect("the probe's own body is the capabilities document");
    assert_eq!(caps.supported_modes, vec!["img_gen"]);
    assert_eq!(caps.limits.max_width, Some(4096));
    assert_eq!(caps.samplers.len(), 21);
    // The start's warnings are still gone with the process that made them —
    // "unknown" rather than a stale copy.
    assert!(view[0].warnings.is_empty(), "{:?}", view[0].warnings);
}

/// Audio's container command is the same string for every audio model
/// (`server --config /config/server.json`) — the model selection lives
/// entirely in the mounted `server.json`. So argv equality alone would adopt a
/// container running a *different* model's config, and adoption compares the
/// rendered config too (§3.4/§3.6).
#[tokio::test]
async fn audio_adoption_compares_the_mounted_config_not_just_the_argv() {
    let health = container_at(200).await;
    let port = health.address().port();
    let data_dir = tempfile::tempdir().unwrap();
    let name = container_name("lmgw", Class::Audio, "tts");

    let started_as = audio_runtime("tts", "qwen3_tts");
    let started_spec = AcquireSpec {
        runtime: &started_as,
        container_prefix: "lmgw",
        models_dir: "/srv/audio",
        data_dir: data_dir.path(),
        may_write_models_dir: true,
        load_timeout: Duration::from_millis(2_000),
        stop_timeout: Duration::from_millis(500),
    };
    // Rendering writes this model's `server.json` — exactly what a real start
    // would have left behind.
    let render = started_as
        .render_spec("lmgw", port, "/srv/audio", data_dir.path())
        .unwrap();
    let cmd = render_engine_args(&render);

    let podman = Arc::new(Podman::default());
    podman.add(FakeContainer {
        name: name.clone(),
        labels: managed(Class::Audio, "tts"),
        state: "running".into(),
        cmd: cmd.clone(),
        host_port: port,
    });
    let reg = registry(podman.clone(), vec![]);
    let report = reg.reconcile("lmgw", &[started_spec]).await;
    assert_eq!(report.adopted, vec![name.clone()], "{:?}", report.removed);

    // Now the row changes underneath it. The argv is byte-identical — only the
    // config the container has mounted is stale.
    let edited = audio_runtime("tts", "pocket_tts");
    let edited_spec = AcquireSpec {
        runtime: &edited,
        container_prefix: "lmgw",
        models_dir: "/srv/audio",
        data_dir: data_dir.path(),
        may_write_models_dir: true,
        load_timeout: Duration::from_millis(2_000),
        stop_timeout: Duration::from_millis(500),
    };
    let reg = registry(podman.clone(), vec![]);
    let report = reg.reconcile("lmgw", &[edited_spec]).await;
    assert!(report.adopted.is_empty());
    assert_eq!(report.removed.len(), 1);
    assert!(
        report.removed[0].1.contains("server.json"),
        "{}",
        report.removed[0].1
    );
    assert!(podman.names().is_empty());
}

// ---------------------------------------------------------------------------
// The app-level lifecycle
// ---------------------------------------------------------------------------

struct Fixture {
    state: SharedState,
    podman: Arc<Podman>,
    _models_dir: tempfile::TempDir,
    _containers: Vec<MockServer>,
}

/// A gateway with `models` chat rows, a stateful fake podman, and one healthy
/// wiremock per model handed out in start order.
async fn fixture(models: &[(&str, i64, bool)]) -> Fixture {
    fixture_with(models, models.len().max(1)).await
}

/// [`fixture`] with exactly `containers` wiremocks to start on — zero makes
/// any start fail loudly, which is how a test says "nothing may start".
async fn fixture_with(models: &[(&str, i64, bool)], containers: usize) -> Fixture {
    let state = AppState::init_for_tests().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let podman = Arc::new(Podman::default());

    let mut mocks = Vec::new();
    let mut ports = std::collections::VecDeque::new();
    for _ in 0..containers {
        let c = container_at(200).await;
        ports.push_back(c.address().port());
        mocks.push(c);
    }
    let queue = Arc::new(Mutex::new(ports));
    state.set_runtime_for_tests(Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || {
            queue.lock().unwrap().pop_front().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "test allocator ran out of ports — an unexpected extra start",
                )
            })
        }),
    )));

    let mut s = Settings::default();
    s.router.models_dir = dir.path().display().to_string();
    // Long enough that a healthy mock always answers, short enough that a
    // genuinely broken start fails the test instead of hanging it.
    s.vram.load_timeout_seconds = 5;
    s.vram.unload_timeout_seconds = 5;
    store::save_settings(&state.db, &s).await.unwrap();

    for (model_id, idle_seconds, warm_start) in models {
        std::fs::write(dir.path().join(format!("{model_id}.gguf")), b"not a gguf").unwrap();
        store::insert_local_model(
            &state.db,
            &NewLocalModel {
                model_id: (*model_id).into(),
                gguf_path: format!("{model_id}.gguf"),
                params: Default::default(),
                args: vec![],
                idle_seconds: *idle_seconds,
                enabled: true,
                public: true,
                image: None,
                extra_run_args: None,
                warm_start: *warm_start,
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
                capabilities_override: None,
                ladder: vec![],
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();

    Fixture {
        state,
        podman,
        _models_dir: dir,
        _containers: mocks,
    }
}

/// The live spec for one configured model — what a test needs to take a real
/// in-flight claim through `acquire`.
async fn acquire_guard(
    state: &SharedState,
    model_id: &str,
) -> lmgw_core::runtime::registry::AcquireGuard {
    let snap = state.snapshot();
    let rt = model_runtime(&snap, Class::Chat, model_id).expect("configured model");
    let spec = lifecycle::acquire_spec(state, &snap, &rt);
    state.runtime().acquire(&spec).await.expect("acquire")
}

#[tokio::test]
async fn boot_warm_starts_only_the_flagged_models() {
    let f = fixture(&[("warm", 0, true), ("cold", 0, false)]).await;
    lifecycle::boot(&f.state).await;

    let started: Vec<String> = f
        .podman
        .verb("run")
        .into_iter()
        .map(|argv| flag_value(&argv, "--name").unwrap_or_default().to_string())
        .collect();
    assert_eq!(started, vec![container_name("lmgw", Class::Chat, "warm")]);

    let live = f.state.runtime().list();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].model_id, "warm");
    assert_eq!(live[0].state, RuntimeState::Ready);
    // Warm means resident, not claimed: nothing holds the model, so the reaper
    // and the eviction policy can both act on it.
    assert_eq!(live[0].in_flight, 0);
}

/// The pre-upgrade settings blob, in the shape §6 migrated away from: the
/// four router-mode keys per class, and the container names the sweep is the
/// last consumer of. Written raw (not through `save_settings`, which can only
/// produce the *new* shape) so the migration in `load_settings` is what this
/// test actually exercises.
fn legacy_settings_json(models_dir: &str, chat: &str, aux: &str, audio: &str) -> String {
    serde_json::json!({
        "bind_addr": "127.0.0.1:8001",
        "router": {
            "image": "ghcr.io/ggml-org/llama.cpp:server-cuda",
            "container_name": chat,
            "listen_port": 9292,
            "models_dir": models_dir,
            "extra_run_args": [],
            "models_max": 0,
            "public_prefix": "",
            "auto_start": true,
        },
        "aux_router": {
            "image": "ghcr.io/ggml-org/llama.cpp:server-cuda",
            "container_name": aux,
            "listen_port": 9293,
            "models_dir": models_dir,
            "extra_run_args": [],
            "models_max": 0,
            "public_prefix": "embed",
            "auto_start": false,
        },
        "audio": {
            "image": "ghcr.io/0xshug0/audio.cpp:full-cuda12",
            "container_name": audio,
            "listen_port": 9294,
            "models_dir": models_dir,
            "backend": "cuda",
            "device": 0,
            "threads": 1,
            "lazy_load": true,
            "extra_run_args": [],
            "public_prefix": "audio",
            "auto_start": false,
        },
    })
    .to_string()
}

/// The §6 migration end to end: an old-shape blob loads clean, its three
/// container names — *custom* ones, not the defaults, which is the case a
/// derived name could never reconstruct — reach the sweep, the sweep removes
/// them, and the settings row it writes back is in the new shape with the
/// legacy list consumed.
#[tokio::test]
async fn boot_sweeps_the_migrated_router_mode_names_and_then_forgets_them() {
    let f = fixture(&[("warm", 0, false)]).await;
    let dir = f.state.snapshot().settings.router.models_dir.clone();
    let (chat, aux, audio) = ("acme-chat", "acme-embed", "acme-audiocpp");
    store::set_kv(
        &f.state.db,
        "settings",
        &legacy_settings_json(&dir, chat, aux, audio),
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let s = f.state.snapshot().settings.clone();
    assert_eq!(
        s.legacy_container_names,
        vec![chat.to_string(), aux.to_string(), audio.to_string()],
        "the load-time migration captures the old names, in class order"
    );
    assert_eq!(s.aux_router.public_prefix, "embed", "the rest still loads");

    for name in [chat, aux, audio] {
        f.podman.add(FakeContainer {
            name: name.into(),
            labels: HashMap::new(),
            state: "running".into(),
            cmd: vec!["--router".into()],
            host_port: 1,
        });
    }

    lifecycle::boot(&f.state).await;

    assert_eq!(
        f.podman.removed(),
        vec![chat.to_string(), aux.to_string(), audio.to_string()],
        "every migrated name is swept, in the order it was captured"
    );
    assert!(
        f.podman.stopped().contains(&chat.to_string()),
        "the sweep stops before it removes: {:?}",
        f.podman.stopped()
    );

    // Consumed: the list is cleared and persisted, which is also the save that
    // finally drops the four router-mode keys from the stored blob.
    assert!(
        f.state
            .snapshot()
            .settings
            .legacy_container_names
            .is_empty(),
        "a clean sweep consumes the list"
    );
    let stored = store::get_kv(&f.state.db, "settings")
        .await
        .unwrap()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert!(
        v.get("legacy_container_names").is_none(),
        "an empty list is not written at all: {stored}"
    );
    for section in ["router", "aux_router", "audio"] {
        for gone in ["container_name", "listen_port", "models_max", "auto_start"] {
            assert!(
                v[section].get(gone).is_none(),
                "`{section}.{gone}` survived the rewrite: {stored}"
            );
        }
    }

    // And a second boot has nothing left to do.
    let before = f.podman.removed().len();
    lifecycle::boot(&f.state).await;
    assert_eq!(f.podman.removed().len(), before, "the sweep is one-time");
}

#[tokio::test]
async fn boot_never_sweeps_a_name_reconciliation_just_adopted() {
    let f = fixture(&[("warm", 0, true)]).await;
    let adopted = container_name("lmgw", Class::Chat, "warm");

    // Paranoia (§3.4): a pre-upgrade `container_name` that collides with a
    // per-model container reconciliation is about to adopt.
    let mut s = f.state.snapshot().settings.clone();
    s.legacy_container_names = vec![adopted.clone(), "acme-audiocpp".into()];
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    f.podman.add(FakeContainer {
        name: "acme-audiocpp".into(),
        labels: HashMap::new(),
        state: "running".into(),
        cmd: vec!["--router".into()],
        host_port: 1,
    });
    let health = container_at(200).await;
    f.podman.add(FakeContainer {
        name: adopted.clone(),
        labels: managed_now(&f, Class::Chat, "warm"),
        state: "running".into(),
        cmd: {
            let snap = f.state.snapshot();
            let rt = model_runtime(&snap, Class::Chat, "warm").unwrap();
            let render = rt
                .render_spec(
                    "lmgw",
                    health.address().port(),
                    &snap.settings.router.models_dir,
                    &f.state.data_dir,
                )
                .unwrap();
            render_engine_args(&render)
        },
        host_port: health.address().port(),
    });

    lifecycle::boot(&f.state).await;

    assert_eq!(
        f.podman.removed(),
        vec!["acme-audiocpp".to_string()],
        "the colliding name is skipped, the real legacy one still goes"
    );
    assert!(
        f.podman.names().contains(&adopted),
        "a name reconciliation just adopted is never swept"
    );
    // Adopted, so no start: the warm-start pass skips a model already resident.
    assert!(f.podman.verb("run").is_empty(), "{:?}", f.podman.calls());
    // A skipped name keeps the list alive: nothing else could rediscover it.
    assert_eq!(
        f.state.snapshot().settings.legacy_container_names,
        vec![adopted, "acme-audiocpp".to_string()],
        "a partial sweep must not consume the list"
    );
}

/// A hold that survived a restart is honored right at boot, immediately after
/// `legacy_sweep` (gpu-hold design §5): reconciliation has just adopted
/// whatever podman was still running, and that is where this container is
/// handed straight back to the sweep. `boot` then **returns** — the warm-start
/// block below must never run at all, not even skip its candidates one by one
/// via `Fit::Held`, because `boot` is spawned rather than awaited and a
/// `join_all` over the warm list would delay the free.
#[tokio::test]
async fn boot_stops_an_adopted_container_and_skips_warm_starts_when_hold_is_persisted() {
    let f = fixture(&[("warm", 0, true)]).await;

    let mut s = f.state.snapshot().settings.clone();
    s.hold.active = true;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();

    let adopted = container_name("lmgw", Class::Chat, "warm");
    let health = container_at(200).await;
    f.podman.add(FakeContainer {
        name: adopted.clone(),
        labels: managed_now(&f, Class::Chat, "warm"),
        state: "running".into(),
        cmd: {
            let snap = f.state.snapshot();
            let rt = model_runtime(&snap, Class::Chat, "warm").unwrap();
            let render = rt
                .render_spec(
                    "lmgw",
                    health.address().port(),
                    &snap.settings.router.models_dir,
                    &f.state.data_dir,
                )
                .unwrap();
            render_engine_args(&render)
        },
        host_port: health.address().port(),
    });

    lifecycle::boot(&f.state).await;

    assert_eq!(
        f.podman.stopped(),
        vec![adopted],
        "the adopted container is handed straight back to the hold sweep"
    );
    assert!(
        f.podman.verb("run").is_empty(),
        "the warm-start model must never run under a persisted hold: {:?}",
        f.podman.calls()
    );
}

#[tokio::test]
async fn the_reaper_stops_an_idle_model_and_leaves_busy_and_never_idle_alone() {
    let f = fixture(&[("idle", 1, true), ("busy", 1, true), ("never", 0, true)]).await;
    lifecycle::boot(&f.state).await;
    assert_eq!(f.state.runtime().list().len(), 3);

    // A request arrives on `busy` and is still running when the reaper looks.
    let claim = acquire_guard(&f.state, "busy").await;

    // `last_used` is compared in whole seconds, so this has to be a real one.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    lifecycle::reap_idle(&f.state).await;

    let live: Vec<String> = f
        .state
        .runtime()
        .list()
        .into_iter()
        .map(|v| v.model_id)
        .collect();
    assert_eq!(live, vec!["busy".to_string(), "never".to_string()]);
    assert_eq!(
        f.podman.stopped(),
        vec![container_name("lmgw", Class::Chat, "idle")]
    );

    // Once the claim drops, the same model becomes reapable on a later tick —
    // the refusal was about the in-flight request, not about the model.
    drop(claim);
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    lifecycle::reap_idle(&f.state).await;
    let live: Vec<String> = f
        .state
        .runtime()
        .list()
        .into_iter()
        .map(|v| v.model_id)
        .collect();
    assert_eq!(live, vec!["never".to_string()]);
}

#[tokio::test]
async fn shutdown_stops_every_managed_container_and_empties_the_registry() {
    let f = fixture(&[("a", 0, true), ("b", 0, true)]).await;
    lifecycle::boot(&f.state).await;
    assert_eq!(f.state.runtime().list().len(), 2);

    // Forced: a busy model is stopped anyway, because nothing is left alive to
    // stop it later (§3.4).
    let _claim = acquire_guard(&f.state, "a").await;
    lifecycle::shutdown(&f.state).await;

    assert!(f.state.runtime().list().is_empty());
    let mut stopped = f.podman.stopped();
    stopped.sort();
    assert_eq!(
        stopped,
        vec![
            container_name("lmgw", Class::Chat, "a"),
            container_name("lmgw", Class::Chat, "b"),
        ]
    );
    // Stopped, not removed: the containers keep their logs, and boot
    // reconciliation collects them next time (they are no longer running).
    assert!(f.podman.removed().is_empty(), "{:?}", f.podman.removed());
}

#[tokio::test]
async fn deleting_a_model_removes_its_container_now() {
    let f = fixture(&[("doomed", 0, true), ("keeper", 0, true)]).await;
    lifecycle::boot(&f.state).await;
    let doomed = container_name("lmgw", Class::Chat, "doomed");
    assert!(f.podman.names().contains(&doomed));

    let out = ops::local_model_set(
        &f.state,
        LocalModelPatch {
            action: "delete".into(),
            model_id: Some("doomed".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], true);
    assert!(
        out["message"].as_str().unwrap().contains("removed"),
        "{}",
        out["message"]
    );

    assert_eq!(f.podman.stopped(), vec![doomed.clone()]);
    assert_eq!(f.podman.removed(), vec![doomed.clone()]);
    assert_eq!(
        f.podman.names(),
        vec![container_name("lmgw", Class::Chat, "keeper")]
    );
    let live: Vec<String> = f
        .state
        .runtime()
        .list()
        .into_iter()
        .map(|v| v.model_id)
        .collect();
    assert_eq!(live, vec!["keeper".to_string()]);
}

#[tokio::test]
async fn disabling_a_model_removes_its_container_and_updating_a_busy_one_does_not() {
    let f = fixture(&[("edited", 0, true), ("switched-off", 0, true)]).await;
    lifecycle::boot(&f.state).await;

    // A plain update on a running, idle model: stopped, so the next request
    // starts it on the new argv (§3.6).
    let out = ops::local_model_set(
        &f.state,
        LocalModelPatch {
            action: "update".into(),
            model_id: Some("edited".into()),
            ctx_size: Some(8192),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        out["message"].as_str().unwrap().contains("stopped"),
        "{}",
        out["message"]
    );
    assert!(!f
        .state
        .runtime()
        .list()
        .iter()
        .any(|v| v.model_id == "edited"));
    // Stopped, not removed — the model still exists and `--replace` collects
    // the husk on its next start.
    assert!(f.podman.removed().is_empty(), "{:?}", f.podman.removed());

    // Disable: the container goes entirely.
    let off = container_name("lmgw", Class::Chat, "switched-off");
    let out = ops::local_model_set(
        &f.state,
        LocalModelPatch {
            action: "disable".into(),
            model_id: Some("switched-off".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        out["message"].as_str().unwrap().contains("removed"),
        "{}",
        out["message"]
    );
    assert_eq!(f.podman.removed(), vec![off.clone()]);
    assert!(!f.podman.names().contains(&off));
}

/// Renaming a model is a delete as far as the runtime is concerned, and the
/// busy-refusal does not apply to it.
///
/// The container's name carries the model id (§3.3), so after a rename nothing
/// renders the old name any more: `--replace` will never collect it, the idle
/// reaper cannot see it (the entry is keyed on a model id no row has), and
/// boot reconciliation removes it only at the *next* boot. Refusing to stop it
/// because a request is in flight would therefore protect a container that the
/// configuration no longer admits to having — and leave it holding VRAM until
/// the process restarts. It goes now, forced, and the in-flight request finds
/// out the honest way.
#[tokio::test]
async fn renaming_a_busy_model_takes_its_old_container_with_it() {
    let f = fixture(&[("old-name", 0, true)]).await;
    lifecycle::boot(&f.state).await;
    let old = container_name("lmgw", Class::Chat, "old-name");
    assert!(f.podman.names().contains(&old));

    let id = f
        .state
        .snapshot()
        .local_models
        .iter()
        .find(|m| m.model_id == "old-name")
        .expect("configured")
        .id;
    let claim = acquire_guard(&f.state, "old-name").await;
    let dead_port = claim.port();

    let out = ops::local_model_set(
        &f.state,
        LocalModelPatch {
            action: "update".into(),
            id: Some(id),
            model_id: Some("new-name".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let message = out["message"].as_str().unwrap();
    assert!(
        message.contains("renamed") && message.contains("removed"),
        "the rename has to say what happened to the old container: {message}"
    );
    assert!(
        !message.contains("still serving"),
        "a rename does not refuse: {message}"
    );
    assert_eq!(
        f.podman.removed(),
        vec![old.clone()],
        "the orphaned container is stopped and removed, not left running"
    );
    assert!(
        !f.podman.names().contains(&old),
        "still there: {:?}",
        f.podman.names()
    );
    assert!(
        f.state.runtime().list().is_empty(),
        "and lmgw no longer believes anything is running: {:?}",
        f.state.runtime().list()
    );

    // The request that was in flight is not lied to: its endpoint is gone, and
    // dropping its (now orphaned) claim touches nothing.
    assert!(
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{dead_port}/health"))
            .timeout(Duration::from_millis(500))
            .send()
            .await
            .is_ok(),
        "the fake container mock is still up — this test asserts lmgw's bookkeeping, \
         not the mock's"
    );
    drop(claim);
    assert!(f.state.runtime().list().is_empty());

    // And the model under its new name is a cold start on a name of its own.
    let renamed = container_name("lmgw", Class::Chat, "new-name");
    assert_ne!(renamed, old);
    assert!(f
        .state
        .snapshot()
        .local_models
        .iter()
        .any(|m| m.model_id == "new-name"));
}

/// The same rule on the aux plane, which has its own CRUD (`web::api`): three
/// classes, one behaviour. Driven over the real op endpoint, because that is
/// the only way in.
#[tokio::test]
async fn renaming_a_running_aux_model_takes_its_old_container_with_it() {
    let f = fixture(&[]).await;
    let dir = f.state.snapshot().settings.router.models_dir.clone();
    let mut s = f.state.snapshot().settings.clone();
    s.aux_router.models_dir = dir.clone();
    store::save_settings(&f.state.db, &s).await.unwrap();
    std::fs::write(std::path::Path::new(&dir).join("e1.gguf"), b"not a gguf").unwrap();
    store::insert_aux_model(
        &f.state.db,
        &lmgw_core::store::NewAuxModel {
            model_id: "e1".into(),
            gguf_path: "e1.gguf".into(),
            kind: lmgw_core::config::AuxKind::Embed,
            pooling: None,
            ctx_size: None,
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
    f.state.reload_snapshot().await.unwrap();
    let id = f.state.snapshot().aux_models[0].id;

    crate::common::container_wire(
        ops::container(&f.state, Some("aux"), Some("e1"), "start", false, None)
            .await
            .unwrap(),
    );
    let old = container_name("lmgw", Class::Aux, "e1");
    assert!(f.podman.names().contains(&old));

    let base = common::serve(f.state.clone()).await;
    let body: serde_json::Value = base
        .client()
        .post(format!("{base}/api/op/aux_model_set"))
        .json(&json!({"action": "update", "id": id, "model_id": "e2"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.contains("renamed"), "{body}");
    assert_eq!(f.podman.removed(), vec![old.clone()]);
    assert!(!f.podman.names().contains(&old));
    assert!(f.state.runtime().list().is_empty());
}

/// A `podman inspect` that fails for a reason other than "no such container"
/// says nothing about whether the container is there — and the sweep list is
/// the only place a router-mode container's name still exists. Retiring it on
/// that answer would leave a container running with nothing left in the system
/// that could ever rediscover it.
#[tokio::test]
async fn the_legacy_sweep_keeps_its_list_when_podman_cannot_answer() {
    let f = fixture(&[("cold", 0, false)]).await;
    let dir = f.state.snapshot().settings.router.models_dir.clone();
    let (chat, aux, audio) = ("acme-chat", "acme-embed", "acme-audiocpp");
    store::set_kv(
        &f.state.db,
        "settings",
        &legacy_settings_json(&dir, chat, aux, audio),
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    *f.podman.inspect_broken.lock().unwrap() =
        Some("Error: unable to connect to Podman socket".into());

    lifecycle::boot(&f.state).await;

    assert!(
        f.podman.removed().is_empty(),
        "nothing may be removed on an answer lmgw could not read: {:?}",
        f.podman.removed()
    );
    assert_eq!(
        f.state.snapshot().settings.legacy_container_names,
        vec![chat.to_string(), aux.to_string(), audio.to_string()],
        "the list survives for the next boot"
    );

    // Once podman can answer again, the same list is swept and consumed.
    *f.podman.inspect_broken.lock().unwrap() = None;
    for name in [chat, aux, audio] {
        f.podman.add(FakeContainer {
            name: name.into(),
            labels: HashMap::new(),
            state: "running".into(),
            cmd: vec!["--router".into()],
            host_port: 1,
        });
    }
    lifecycle::boot(&f.state).await;
    assert_eq!(
        f.podman.removed(),
        vec![chat.to_string(), aux.to_string(), audio.to_string()]
    );
    assert!(f
        .state
        .snapshot()
        .settings
        .legacy_container_names
        .is_empty());
}

#[tokio::test]
async fn updating_a_busy_model_says_so_instead_of_cutting_the_request_short() {
    let f = fixture(&[("busy", 0, true)]).await;
    lifecycle::boot(&f.state).await;
    let _claim = acquire_guard(&f.state, "busy").await;

    let out = ops::local_model_set(
        &f.state,
        LocalModelPatch {
            action: "update".into(),
            model_id: Some("busy".into()),
            ctx_size: Some(8192),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let message = out["message"].as_str().unwrap();
    assert!(message.contains("still serving 1 request"), "{message}");
    assert!(
        f.state
            .runtime()
            .list()
            .iter()
            .any(|v| v.model_id == "busy" && v.state == RuntimeState::Ready),
        "a model mid-request keeps running the configuration it was started with"
    );
    assert!(f.podman.stopped().is_empty(), "{:?}", f.podman.stopped());
}

// ---------------------------------------------------------------------------
// Ladder rows (ladder design §3.1, §3.5, §4; §12 entries 10 and 19–30)
// ---------------------------------------------------------------------------

const LADDER: &str = "laddered";

/// Add a three-rung ladder row (ladder design §4.1): the base, then two higher
/// rungs with their own weights and context. One slot, so each rung's per-slot
/// context is its `ctx_size`. The files are not GGUFs — nothing here reads a
/// header, it only compares command lines.
async fn add_ladder_row(f: &Fixture, idle_seconds: i64) {
    let dir = f.state.snapshot().settings.router.models_dir.clone();
    for file in ["ladder-base.gguf", "ladder-mid.gguf", "ladder-top.gguf"] {
        std::fs::write(Path::new(&dir).join(file), b"not a gguf").unwrap();
    }
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: LADDER.into(),
            gguf_path: "ladder-base.gguf".into(),
            params: LlamaParams {
                ctx_size: Some(4096),
                parallel: Some(1),
                n_predict: Some(256),
                ..Default::default()
            },
            args: vec![],
            idle_seconds,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: None,
            ladder: vec![
                lmgw_core::ladder::Rung {
                    gguf_path: "ladder-mid.gguf".into(),
                    ctx_size: 8192,
                },
                lmgw_core::ladder::Rung {
                    gguf_path: "ladder-top.gguf".into(),
                    ctx_size: 16384,
                },
            ],
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// The container command the ladder row renders at `rung` (0-based) on `port`.
fn ladder_cmd(f: &Fixture, rung: usize, port: u16) -> Vec<String> {
    let snap = f.state.snapshot();
    let rt = lmgw_core::runtime::descriptor::model_runtime_at(&snap, Class::Chat, LADDER, rung)
        .expect("the ladder row");
    let render = rt
        .render_spec(
            "lmgw",
            port,
            &snap.settings.router.models_dir,
            &f.state.data_dir,
        )
        .unwrap();
    render_engine_args(&render)
}

/// What a previous lmgw left behind after climbing the ladder row to `rung`
/// (0-based): a running container with that rung's command line, which boot
/// then reconciles. The returned mock is the container's port; keep it.
async fn left_running_at(f: &Fixture, rung: usize) -> MockServer {
    let health = container_at(200).await;
    let port = health.address().port();
    f.podman.add(FakeContainer {
        name: container_name("lmgw", Class::Chat, LADDER),
        labels: managed_now(f, Class::Chat, LADDER),
        state: "running".into(),
        cmd: ladder_cmd(f, rung, port),
        host_port: port,
    });
    lifecycle::boot(&f.state).await;
    health
}

/// The weights file and `--ctx-size` of every `podman run`, in order.
fn started_rungs(f: &Fixture) -> Vec<(String, String)> {
    f.podman
        .verb("run")
        .iter()
        .map(|argv| {
            let file = flag_value(argv, "-m")
                .and_then(|m| m.rsplit('/').next())
                .unwrap_or_default()
                .to_string();
            let ctx = flag_value(argv, "--ctx-size")
                .unwrap_or_default()
                .to_string();
            (file, ctx)
        })
        .collect()
}

fn base() -> (String, String) {
    ("ladder-base.gguf".into(), "4096".into())
}

/// The next start of the ladder model, through the request path's own
/// admission — which is where "every start is the base" (§3.5) has to hold.
async fn next_request_start(f: &Fixture) -> lmgw_core::vram::LocalHold {
    let route = f.state.snapshot().resolve(LADDER).unwrap();
    lmgw_core::vram::admit(&f.state, &route, LADDER)
        .await
        .expect("admitted")
        .expect("a local model is held")
}

/// §3.1 "lmgw killed mid-climb": a crash is not a container stop, so a
/// container a previous lmgw had climbed is adopted at the rung its command
/// line matches, and everything that reads the entry — the status view, the
/// ledger's charge, a request's gate facts — sees that rung, not the base.
#[tokio::test]
async fn boot_adopts_a_climbed_container_at_the_rung_it_runs() {
    // No container to start: adoption must start nothing.
    let f = fixture_with(&[], 0).await;
    add_ladder_row(&f, 0).await;
    let _running = left_running_at(&f, 1).await;

    let live = f.state.runtime().list();
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(live[0].state, RuntimeState::Ready);
    let rung = live[0]
        .rung
        .as_ref()
        .expect("a ladder entry shows its rung");
    assert_eq!((rung.rung, rung.of), (2, 3), "1-based, like every surface");
    assert_eq!(rung.gguf, "ladder-mid.gguf");
    let charge = live[0].charge.as_ref().expect("and is charged for it");
    assert_eq!(charge.index, 1);
    assert_eq!(charge.gguf_path, "ladder-mid.gguf");
    assert_eq!(charge.ctx_size, Some(8192));
    let frame = serde_json::to_value(&live[0]).unwrap();
    assert_eq!(
        frame["rung"],
        json!({"rung": 2, "of": 3, "gguf": "ladder-mid.gguf"}),
        "{frame}"
    );
    assert!(frame.get("charge").is_none(), "the charge is not published");
    assert!(f.podman.verb("run").is_empty(), "{:?}", f.podman.calls());

    // A request's claim is on the adopted rung: its gate reads rung 2's facts.
    let hold = next_request_start(&f).await;
    let facts = hold.gate_facts().expect("a chat claim carries its facts");
    assert_eq!(
        facts.rung,
        Some(lmgw_core::runtime::descriptor::RungPos { index: 1, of: 3 })
    );
    assert_eq!(facts.gguf_path, "ladder-mid.gguf");
    assert_eq!(facts.gguf_file(), "ladder-mid.gguf");
    assert_eq!(facts.params.ctx_size, Some(8192));
    assert_eq!(facts.per_request_ctx(), Some(8192));
    assert_eq!(hold.rung().map(|r| r.index), Some(1));
}

/// A ladder container whose command line is none of the row's rungs — the
/// ladder was edited while lmgw was down — is removed like any stale
/// container, and the next start is the base.
#[tokio::test]
async fn boot_removes_a_ladder_container_that_matches_no_rung() {
    let f = fixture_with(&[], 1).await;
    add_ladder_row(&f, 0).await;
    let stale = container_at(200).await;
    let port = stale.address().port();
    let mut cmd = ladder_cmd(&f, 2, port);
    let at = cmd.iter().position(|a| a == "--ctx-size").unwrap();
    cmd[at + 1] = "12345".into();
    let name = container_name("lmgw", Class::Chat, LADDER);
    f.podman.add(FakeContainer {
        name: name.clone(),
        labels: managed_now(&f, Class::Chat, LADDER),
        state: "running".into(),
        cmd,
        host_port: port,
    });

    lifecycle::boot(&f.state).await;
    assert!(f.state.runtime().list().is_empty());
    assert_eq!(f.podman.removed(), vec![name]);

    let _hold = next_request_start(&f).await;
    assert_eq!(started_rungs(&f), vec![base()]);
}

/// A row without a ladder publishes exactly the frame it always did: no rung,
/// no climb, no send count (§12 entry 11's surfaces are ladder-only).
#[tokio::test]
async fn a_row_without_a_ladder_keeps_its_runtime_frame() {
    let f = fixture(&[("plain", 0, true)]).await;
    lifecycle::boot(&f.state).await;
    let frame = serde_json::to_value(&f.state.runtime().list()[0]).unwrap();
    let mut keys: Vec<&str> = frame
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "class",
            "container_name",
            "in_flight",
            "last_used_age_seconds",
            "llama_props",
            "model_id",
            "port",
            "started_at_age_seconds",
            "state",
        ],
        "{frame}"
    );
    assert!(f.state.runtime().list()[0].charge.is_none());
}

/// §3.5 "coming down only through a container stop": the idle reaper stops
/// a climbed model, and the next request starts the **base**.
#[tokio::test]
async fn the_reaper_resets_a_climbed_ladder_to_its_base() {
    let f = fixture_with(&[], 1).await;
    add_ladder_row(&f, 1).await;
    let _running = left_running_at(&f, 2).await;
    assert_eq!(f.state.runtime().list()[0].rung.as_ref().unwrap().rung, 3);

    // `last_used` is compared in whole seconds.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    lifecycle::reap_idle(&f.state).await;
    assert!(f.state.runtime().list().is_empty());
    assert_eq!(
        f.podman.stopped(),
        vec![container_name("lmgw", Class::Chat, LADDER)]
    );

    let hold = next_request_start(&f).await;
    assert_eq!(started_rungs(&f), vec![base()]);
    assert_eq!(hold.rung().map(|r| r.index), Some(0));
    assert_eq!(f.state.runtime().list()[0].rung.as_ref().unwrap().rung, 1);
}

/// The hold's sweep stops a climbed model; once the hold is released the
/// next request starts the base.
#[tokio::test]
async fn the_hold_sweep_resets_a_climbed_ladder_to_its_base() {
    let f = fixture_with(&[], 1).await;
    add_ladder_row(&f, 0).await;
    let _running = left_running_at(&f, 2).await;

    ops::hold_set(&f.state, true).await.unwrap();
    assert!(f.state.runtime().list().is_empty());
    ops::hold_set(&f.state, false).await.unwrap();

    let _hold = next_request_start(&f).await;
    assert_eq!(started_rungs(&f), vec![base()]);
}

/// An edit's apply stops a climbed model; the next request starts the base
/// with the new configuration.
#[tokio::test]
async fn an_apply_resets_a_climbed_ladder_to_its_base() {
    let f = fixture_with(&[], 1).await;
    add_ladder_row(&f, 0).await;
    let _running = left_running_at(&f, 1).await;

    let said = lifecycle::stop_for_apply(&f.state, Class::Chat, LADDER).await;
    assert!(said.unwrap().contains("was stopped"));
    assert!(f.state.runtime().list().is_empty());

    let _hold = next_request_start(&f).await;
    assert_eq!(started_rungs(&f), vec![base()]);
}

/// `restart` is "back to base" (§3.5): the operator's way down. It stops the
/// climbed container and starts the base at once.
#[tokio::test]
async fn restart_brings_a_climbed_ladder_back_to_its_base() {
    let f = fixture_with(&[], 1).await;
    add_ladder_row(&f, 0).await;
    let _running = left_running_at(&f, 2).await;

    let out = crate::common::container_wire(
        ops::container(&f.state, None, Some(LADDER), "restart", false, None)
            .await
            .unwrap(),
    );
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(started_rungs(&f), vec![base()]);
    let live = f.state.runtime().list();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].rung.as_ref().unwrap().rung, 1, "{live:?}");
}

/// An agent's containers carry this instance's label like the model
/// containers, and no model's: the model reconcile must leave them alone —
/// a service container an early App-tab request started while the boot's
/// `podman ps` was on its way, and an exited one whose logs a 503 quotes.
/// The agents' own reconcile collects their leftovers.
#[tokio::test]
async fn the_model_reconcile_leaves_an_agent_s_containers_alone() {
    let podman = Arc::new(Podman::default());
    for (name, run, state) in [
        ("lmgw-agentsvc-folder-chat", "service", "running"),
        ("lmgw-agent-mail-labeler-7", "7", "exited"),
    ] {
        podman.add(FakeContainer {
            name: name.into(),
            labels: HashMap::from([
                ("lmgw.instance".to_string(), "lmgw".to_string()),
                ("lmgw.kind".to_string(), "agent".to_string()),
                ("lmgw.agent".to_string(), "folder-chat".to_string()),
                ("lmgw.run".to_string(), run.to_string()),
            ]),
            state: state.into(),
            cmd: Vec::new(),
            host_port: 0,
        });
    }
    let reg = registry(podman.clone(), vec![]);
    let rt = chat_runtime("m1");

    let report = reg.reconcile("lmgw", &[spec(&rt)]).await;
    assert!(report.listed);
    assert!(report.adopted.is_empty(), "{:?}", report.adopted);
    assert!(report.removed.is_empty(), "{:?}", report.removed);
    assert!(podman.removed().is_empty(), "{:?}", podman.removed());
    assert!(reg.list().is_empty());
}
