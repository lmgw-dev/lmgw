//! The launcher seam (Windows design §4.1): the registry drives a model's
//! process only through its `Launcher`, so a registry built with a launcher
//! that is not Podman starts, watches, stops, lists and attributes through it
//! alone — and never shells `podman` for a model.
//!
//! The fake launcher here records every verb and answers like a launcher
//! whose processes never outlive lmgw (the native one's shape): it lists
//! nothing, so there is nothing to adopt. Readiness stays the registry's own
//! HTTP poll, against a wiremock `/health`.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::erased::Erased;
use lmgw_core::runtime::argv::{LlamaArgs, RenderSpec};
use lmgw_core::runtime::descriptor::ModelRuntime;
use lmgw_core::runtime::launcher::{
    Exited, Found, Launch, LaunchFailure, Launched, Launcher, Listed, Listing, Prepare, Prepared,
    Presence,
};
use lmgw_core::runtime::registry::{
    AcquireSpec, CmdOutput, CommandRunner, Registry, RuntimeError, RuntimeState,
};
use lmgw_core::runtime::{container_name, Class, Placement};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A runner that fails the test if anything shells podman: a model's
/// process is the launcher's business.
struct NoPodman;

impl CommandRunner for NoPodman {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        panic!(
            "the registry shelled `{program} {}` past its launcher",
            args.join(" ")
        )
    }
}

/// One launch the registry asked for.
#[derive(Debug, Clone)]
struct Started {
    name: String,
    port: u16,
    owner: Option<String>,
    placement: Placement,
}

#[derive(Default)]
struct Fake {
    /// Every verb, with the name it was about, in order.
    verbs: Mutex<Vec<(String, String)>>,
    launches: Mutex<Vec<Started>>,
    /// Consumed one per launch; exhausted = success.
    launch_replies: Mutex<VecDeque<Result<(), LaunchFailure>>>,
    /// What `exited` answers.
    exited: Mutex<Option<Exited>>,
    /// What `host_pids` answers.
    pids: Mutex<HashMap<String, u32>>,
    /// What `list` answers (none by default: the native launcher's shape).
    listed: Mutex<Vec<Listed>>,
    /// What `inspect` answers; `None` refuses.
    found: Mutex<Option<Found>>,
    /// Whether `same_start` agrees.
    same: Mutex<bool>,
}

impl Fake {
    fn record(&self, verb: &str, name: &str) {
        self.verbs
            .lock()
            .unwrap()
            .push((verb.to_string(), name.to_string()));
    }

    fn verbs(&self) -> Vec<String> {
        self.verbs
            .lock()
            .unwrap()
            .iter()
            .map(|(v, _)| v.clone())
            .collect()
    }

    fn named(&self, verb: &str) -> Vec<String> {
        self.verbs
            .lock()
            .unwrap()
            .iter()
            .filter(|(v, _)| v == verb)
            .map(|(_, n)| n.clone())
            .collect()
    }
}

const RESTART_NOTE: &str = "the fake launcher says so";

impl Launcher for Fake {
    fn runs_containers(&self) -> bool {
        false
    }

    fn prepare<'a>(&'a self, _prepare: Prepare<'a>) -> Erased<'a, Result<Prepared, String>> {
        Box::pin(async move {
            self.record("prepare", "");
            Ok(Prepared::default())
        })
    }

    fn launch<'a>(&'a self, launch: Launch<'a>) -> Erased<'a, Result<Launched, LaunchFailure>> {
        Box::pin(async move {
            self.record("launch", &launch.render.container_name);
            self.launches.lock().unwrap().push(Started {
                name: launch.render.container_name.clone(),
                port: launch.render.host_port,
                owner: launch.owner.map(str::to_string),
                placement: launch.placement,
            });
            match self.launch_replies.lock().unwrap().pop_front() {
                Some(Err(failure)) => Err(failure),
                _ => Ok(Launched {
                    warnings: vec![RESTART_NOTE.to_string()],
                }),
            }
        })
    }

    fn exited<'a>(&'a self, name: &'a str) -> Erased<'a, Option<Exited>> {
        Box::pin(async move {
            self.record("exited", name);
            self.exited.lock().unwrap().clone()
        })
    }

    fn stop<'a>(&'a self, name: &'a str, _unload_timeout: Duration) -> Erased<'a, Option<String>> {
        Box::pin(async move {
            self.record("stop", name);
            None
        })
    }

    fn stop_quietly<'a>(&'a self, name: &'a str) -> Erased<'a, ()> {
        Box::pin(async move { self.record("stop_quietly", name) })
    }

    fn remove<'a>(&'a self, name: &'a str) -> Erased<'a, Result<(), String>> {
        Box::pin(async move {
            self.record("remove", name);
            Ok(())
        })
    }

    fn logs_tail<'a>(&'a self, name: &'a str, lines: usize) -> Erased<'a, Result<String, String>> {
        Box::pin(async move {
            self.record("logs_tail", name);
            Ok(format!("{lines} lines of {name}\n"))
        })
    }

    fn log_excerpt<'a>(&'a self, name: &'a str) -> Erased<'a, Vec<String>> {
        Box::pin(async move {
            self.record("log_excerpt", name);
            vec!["error: the fake engine gave up".to_string()]
        })
    }

    fn exists<'a>(&'a self, name: &'a str) -> Erased<'a, Presence> {
        Box::pin(async move {
            self.record("exists", name);
            Presence::Absent
        })
    }

    fn list<'a>(
        &'a self,
        _container_prefix: &'a str,
        _which: Listing,
    ) -> Erased<'a, Result<Vec<Listed>, String>> {
        Box::pin(async move {
            self.record("list", "");
            Ok(self.listed.lock().unwrap().clone())
        })
    }

    fn inspect<'a>(&'a self, name: &'a str) -> Erased<'a, Result<Found, String>> {
        Box::pin(async move {
            self.record("inspect", name);
            self.found
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| "nothing outlives lmgw here".to_string())
        })
    }

    fn same_start(
        &self,
        _container_prefix: &str,
        _render: &RenderSpec,
        _found: &Found,
        _labels: &HashMap<String, String>,
    ) -> Result<(), String> {
        match *self.same.lock().unwrap() {
            true => Ok(()),
            false => Err("nothing outlives lmgw here".to_string()),
        }
    }

    fn host_pids<'a>(
        &'a self,
        names: &'a [String],
    ) -> Erased<'a, Result<HashMap<String, u32>, String>> {
        Box::pin(async move {
            self.record("host_pids", &names.join(","));
            let pids = self.pids.lock().unwrap();
            Ok(names
                .iter()
                .filter_map(|n| pids.get(n).map(|p| (n.clone(), *p)))
                .collect())
        })
    }
}

/// A registry over `fake` whose port allocator hands out `ports`, in order.
fn registry(fake: Arc<Fake>, ports: Vec<u16>) -> Arc<Registry> {
    let queue = Arc::new(Mutex::new(VecDeque::from(ports)));
    Arc::new(Registry::with_launcher(
        fake,
        Arc::new(NoPodman),
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

/// A port nothing answers on for the whole test, for an attempt that only
/// the fake's exit report may end: [`free_port`]'s claim, below the kernel's
/// ephemeral range. A `bind(0)` port let go was there for any other test's
/// `bind(0)` to take while the registry polled it, and a wiremock `/health`
/// that did answered 200 for a start that had to fail.
///
/// [`free_port`]: crate::support::device_link::free_port
fn dead_port() -> u16 {
    crate::support::device_link::free_port()
}

async fn healthy() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .mount(&server)
        .await;
    server
}

/// A llama row on the CPU, with no run args: what a launcher that is not
/// Podman gets (no `--device` to read a card from).
fn runtime(model_id: &str) -> ModelRuntime {
    ModelRuntime {
        class: Class::Chat,
        model_id: model_id.into(),
        image: "localhost/llama-server:official-latest".into(),
        extra_run_args: vec![],
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

#[tokio::test]
async fn a_start_and_a_stop_go_through_the_launcher_alone() {
    let health = healthy().await;
    let port = health.address().port();
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![port]);
    reg.set_owner("instance-a".into());
    let rt = runtime("qwen3-2b");
    let name = container_name("lmgw", Class::Chat, "qwen3-2b");

    let guard = reg.acquire(&spec(&rt, 5_000)).await.unwrap();
    assert_eq!(guard.endpoint(), format!("http://127.0.0.1:{port}"));
    let launches = fake.launches.lock().unwrap().clone();
    assert_eq!(launches.len(), 1, "{launches:?}");
    assert_eq!(launches[0].name, name);
    assert_eq!(launches[0].port, port);
    assert_eq!(launches[0].owner.as_deref(), Some("instance-a"));
    assert_eq!(launches[0].placement, rt.placement());
    let view = reg.list();
    assert_eq!(view[0].state, RuntimeState::Ready);
    assert!(
        view[0].warnings.iter().any(|w| w == RESTART_NOTE),
        "a launch's warnings land on the entry: {:?}",
        view[0].warnings
    );
    assert_eq!(fake.verbs()[..2], ["prepare", "launch"]);

    // The log surface reads the launcher's tail.
    assert_eq!(
        reg.logs_tail(&name, 5).await.unwrap(),
        format!("5 lines of {name}\n")
    );

    drop(guard);
    reg.stop(Class::Chat, "qwen3-2b", false).await.unwrap();
    assert_eq!(fake.named("stop"), std::slice::from_ref(&name));
    assert!(reg.list().is_empty());

    // Delete hygiene removes by the rendered name, through the launcher.
    reg.stop_and_remove("lmgw", Class::Chat, "qwen3-2b", false)
        .await
        .unwrap();
    assert_eq!(fake.named("remove"), [name]);
}

#[tokio::test]
async fn a_process_that_exits_while_loading_fails_the_start_with_the_launchers_words() {
    // Nothing answers on this port: only the launcher's exit check can end
    // the poll before the load timeout.
    let port = dead_port();
    let fake = Arc::new(Fake::default());
    *fake.exited.lock().unwrap() = Some(Exited::plain("the engine exited with code 1"));
    let reg = registry(fake.clone(), vec![port]);
    let rt = runtime("dies");
    let name = container_name("lmgw", Class::Chat, "dies");

    let started = std::time::Instant::now();
    let err = reg.acquire(&spec(&rt, 30_000)).await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the exit check ended the poll, not the load timeout"
    );
    match err {
        RuntimeError::Start { message, logs, .. } => {
            assert!(
                message.contains("the engine exited with code 1"),
                "{message}"
            );
            assert_eq!(logs, ["error: the fake engine gave up"]);
        }
        other => panic!("expected a start error, got {other:?}"),
    }
    assert_eq!(fake.named("stop_quietly"), std::slice::from_ref(&name));
    assert_eq!(fake.named("log_excerpt"), [name]);
    assert!(reg.list().is_empty());
}

#[tokio::test]
async fn a_taken_port_is_retried_once_on_a_fresh_one_after_the_launcher_collects_the_husk() {
    let health = healthy().await;
    let port = health.address().port();
    let fake = Arc::new(Fake::default());
    fake.launch_replies
        .lock()
        .unwrap()
        .push_back(Err(LaunchFailure {
            message: "bind: address already in use".into(),
            port_in_use: true,
        }));
    let reg = registry(fake.clone(), vec![1, port]);
    let rt = runtime("retried");
    let name = container_name("lmgw", Class::Chat, "retried");

    reg.acquire(&spec(&rt, 5_000)).await.unwrap();
    let ports: Vec<u16> = fake
        .launches
        .lock()
        .unwrap()
        .iter()
        .map(|l| l.port)
        .collect();
    assert_eq!(ports, [1, port]);
    assert_eq!(fake.named("remove"), [name]);
    assert_eq!(
        fake.verbs().iter().filter(|v| *v == "prepare").count(),
        1,
        "prepared once per start, not per attempt"
    );
}

#[tokio::test]
async fn a_launch_failure_that_is_not_a_taken_port_is_the_start_answer() {
    let fake = Arc::new(Fake::default());
    fake.launch_replies
        .lock()
        .unwrap()
        .push_back(Err(LaunchFailure {
            message: "the engine binary is missing".into(),
            port_in_use: false,
        }));
    let reg = registry(fake.clone(), vec![1, 2]);
    let rt = runtime("missing");

    let err = reg.acquire(&spec(&rt, 5_000)).await.unwrap_err();
    assert!(
        err.to_string().contains("the engine binary is missing"),
        "{err}"
    );
    assert_eq!(fake.launches.lock().unwrap().len(), 1, "no retry");
}

#[tokio::test]
async fn a_launcher_that_lists_nothing_adopts_nothing_at_boot() {
    let fake = Arc::new(Fake::default());
    let reg = registry(fake.clone(), vec![]);
    let rt = runtime("warm");

    let report = reg.reconcile("lmgw", &[spec(&rt, 5_000)]).await;
    assert!(report.listed);
    assert!(report.adopted.is_empty() && report.removed.is_empty());
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(fake.verbs(), ["list"]);
    assert!(reg.list().is_empty());

    // The legacy sweep's question is the launcher's too.
    assert_eq!(
        reg.container_exists("lmgw-llama-server").await,
        Presence::Absent
    );
}

#[tokio::test]
async fn host_pids_are_the_launchers() {
    let fake = Arc::new(Fake::default());
    fake.pids.lock().unwrap().insert("lmgw-chat-a".into(), 4242);
    let reg = registry(fake.clone(), vec![]);

    let pids = reg
        .host_pids(&["lmgw-chat-a".into(), "lmgw-chat-b".into()])
        .await
        .unwrap();
    assert_eq!(pids, HashMap::from([("lmgw-chat-a".to_string(), 4242)]));
}

#[tokio::test]
async fn an_exit_on_a_taken_port_is_retried_once_like_a_launch_that_says_so() {
    // The first port answers nothing: only the exit check ends that attempt.
    let dead = dead_port();
    let health = healthy().await;
    let port = health.address().port();
    let fake = Arc::new(Fake::default());
    *fake.exited.lock().unwrap() = Some(Exited {
        message: "the engine could not bind its port".into(),
        port_in_use: true,
        fallback: None,
    });
    let reg = registry(fake.clone(), vec![dead, port]);
    let rt = runtime("rebound");
    let name = container_name("lmgw", Class::Chat, "rebound");

    // The fake keeps saying "exited" on the second attempt too, but its
    // port answers /health first.
    reg.acquire(&spec(&rt, 30_000)).await.unwrap();
    let ports: Vec<u16> = fake
        .launches
        .lock()
        .unwrap()
        .iter()
        .map(|l| l.port)
        .collect();
    assert_eq!(ports, [dead, port]);
    assert_eq!(fake.named("remove"), [name]);
    assert!(fake.named("stop_quietly").is_empty());
}

#[tokio::test]
async fn a_second_exit_on_a_taken_port_is_the_start_answer() {
    let dead = dead_port();
    let fake = Arc::new(Fake::default());
    *fake.exited.lock().unwrap() = Some(Exited {
        message: "the engine could not bind its port".into(),
        port_in_use: true,
        fallback: None,
    });
    let reg = registry(fake.clone(), vec![dead, dead]);
    let rt = runtime("never");

    let err = reg.acquire(&spec(&rt, 30_000)).await.unwrap_err();
    assert!(err.to_string().contains("could not bind"), "{err}");
    assert_eq!(fake.launches.lock().unwrap().len(), 2, "one retry, no more");
}

#[tokio::test]
async fn an_exit_that_asks_for_the_next_build_is_prepared_and_launched_once_more() {
    let dead = dead_port();
    let health = healthy().await;
    let port = health.address().port();
    let fake = Arc::new(Fake::default());
    *fake.exited.lock().unwrap() = Some(Exited {
        message: "the engine exited with code 1".into(),
        port_in_use: false,
        fallback: Some("its CUDA build has no kernel image here".into()),
    });
    let reg = registry(fake.clone(), vec![dead, port]);
    let rt = runtime("next-build");
    let name = container_name("lmgw", Class::Chat, "next-build");

    reg.acquire(&spec(&rt, 30_000)).await.unwrap();
    let verbs: Vec<String> = fake.verbs().into_iter().filter(|v| v != "exited").collect();
    // The ended attempt is stopped, not removed: its log stays readable
    // until the next launch replaces it.
    assert_eq!(
        verbs,
        ["prepare", "launch", "stop_quietly", "prepare", "launch"]
    );
    assert_eq!(fake.named("stop_quietly"), [name]);
    let entry = reg.list().remove(0);
    assert!(
        entry
            .warnings
            .iter()
            .any(|w| w.contains("exited with code 1")
                && w.ends_with(": its CUDA build has no kernel image here")),
        "{:?}",
        entry.warnings
    );
}

#[tokio::test]
async fn a_start_that_fails_after_its_fallback_says_both() {
    let dead = dead_port();
    let fake = Arc::new(Fake::default());
    *fake.exited.lock().unwrap() = Some(Exited {
        message: "the engine exited with code 1".into(),
        port_in_use: false,
        fallback: Some("no kernel image".into()),
    });
    let reg = registry(fake.clone(), vec![dead, dead]);
    let rt = runtime("still-not");

    let err = reg
        .acquire(&spec(&rt, 30_000))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("after the fallback to the engine's next build"),
        "{err}"
    );
    assert_eq!(
        fake.launches.lock().unwrap().len(),
        2,
        "one fallback, no more"
    );
    assert_eq!(fake.named("prepare").len(), 2);
}

#[tokio::test]
async fn a_listed_instance_that_matches_is_adopted_through_the_launcher() {
    let health = healthy().await;
    let port = health.address().port();
    let fake = Arc::new(Fake::default());
    let name = container_name("lmgw", Class::Chat, "kept");
    let labels = HashMap::from([
        ("lmgw.instance".to_string(), "lmgw".to_string()),
        ("lmgw.class".to_string(), "chat".to_string()),
        ("lmgw.model".to_string(), "kept".to_string()),
    ]);
    *fake.listed.lock().unwrap() = vec![
        Listed {
            names: vec![name.clone()],
            labels: labels.clone(),
            state: "running".into(),
            created: 0,
        },
        // A model no row describes: removed through the launcher.
        Listed {
            names: vec!["lmgw-chat-gone-000000".into()],
            labels: HashMap::from([
                ("lmgw.class".to_string(), "chat".to_string()),
                ("lmgw.model".to_string(), "gone".to_string()),
            ]),
            state: "running".into(),
            created: 0,
        },
    ];
    *fake.found.lock().unwrap() = Some(Found {
        running: Some(true),
        labels,
        cmd: vec![],
        host_port: Some(port),
    });
    *fake.same.lock().unwrap() = true;
    let reg = registry(fake.clone(), vec![]);
    let rt = runtime("kept");

    let report = reg.reconcile("lmgw", &[spec(&rt, 5_000)]).await;
    assert_eq!(report.adopted, std::slice::from_ref(&name));
    assert_eq!(report.removed.len(), 1, "{:?}", report.removed);
    assert_eq!(fake.named("remove"), ["lmgw-chat-gone-000000"]);
    assert_eq!(fake.named("inspect"), [name]);
    let view = reg.list();
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].state, RuntimeState::Ready);
    assert_eq!(view[0].port, port);
}
