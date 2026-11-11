//! The native launcher (Windows design §4.1), driven through the registry
//! with the real launcher against `lmgw-fake-engine`, a stand-in for
//! llama-server that needs nothing downloaded (`fake-engine/main.rs`). It is
//! installed the way a release would be, as
//! `<engines>/llama/fake-b1/llama-server`, and the registry starts, polls,
//! stops and reads it as it would the real one.
//!
//! The live test at the end runs a real llama-server release build on the
//! CPU when `LMGW_LIVE_NATIVE_ENGINES` and `LMGW_LIVE_NATIVE_GGUF` are set
//! (its doc says how), and skips otherwise.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lmgw_core::config::LlamaParams;
use lmgw_core::erased::Erased;
use lmgw_core::runtime::argv::LlamaArgs;
use lmgw_core::runtime::descriptor::ModelRuntime;
use lmgw_core::runtime::engine_assets::{self, Engines};
use lmgw_core::runtime::launcher::native::engines;
use lmgw_core::runtime::launcher::{Launcher, Native};
use lmgw_core::runtime::registry::{
    AcquireSpec, CmdOutput, CommandRunner, Registry, RuntimeError, RuntimeState,
};
use lmgw_core::runtime::{container_name, Class};

const FAKE_ENGINE: &str = env!("CARGO_BIN_EXE_lmgw-fake-engine");

/// A runner that fails the test if anything shells a program for a model:
/// the native launcher starts the engine itself.
struct NoPodman;

impl CommandRunner for NoPodman {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        panic!(
            "the registry shelled `{program} {}` past the native launcher",
            args.join(" ")
        )
    }
}

/// A data dir with the fake engine installed as the llama build, and a
/// registry over a native launcher that finds it there.
struct Rig {
    dir: tempfile::TempDir,
    reg: Arc<Registry>,
}

impl Rig {
    fn new(ports: Vec<u16>) -> Self {
        Self::new_in(tempfile::tempdir().unwrap(), ports)
    }

    fn new_in(dir: tempfile::TempDir, ports: Vec<u16>) -> Self {
        let build = dir.path().join("engines").join("llama").join("fake-b1");
        std::fs::create_dir_all(&build).unwrap();
        install_fake(&build.join(engines::executable(Class::Chat)));
        std::fs::create_dir_all(dir.path().join("models")).unwrap();
        // Hand-installed: a platform with pins (Windows) would otherwise
        // download the pinned llama.cpp build instead.
        let native = Native::with_engines(Engines::hand_installed(engine_assets::engines_root(
            dir.path(),
        )))
        .unwrap();
        let reg = registry(Arc::new(native), ports);
        Self { dir, reg }
    }

    fn models(&self) -> String {
        self.dir.path().join("models").display().to_string()
    }

    fn build_dir(&self) -> PathBuf {
        self.dir
            .path()
            .join("engines")
            .join("llama")
            .join("fake-b1")
    }

    fn spec<'a>(&'a self, models: &'a str, rt: &'a ModelRuntime, load_ms: u64) -> AcquireSpec<'a> {
        AcquireSpec {
            runtime: rt,
            container_prefix: "lmgw",
            models_dir: models,
            data_dir: self.dir.path(),
            may_write_models_dir: true,
            load_timeout: Duration::from_millis(load_ms),
            stop_timeout: Duration::from_secs(5),
        }
    }
}

#[cfg(unix)]
fn install_fake(at: &Path) {
    std::os::unix::fs::symlink(FAKE_ENGINE, at).unwrap();
}

#[cfg(not(unix))]
fn install_fake(at: &Path) {
    std::fs::copy(FAKE_ENGINE, at).unwrap();
}

/// A registry over `launcher` whose port allocator hands out `ports` in
/// order, then [`free_port`]'s claimed ports. Not the product's
/// `ephemeral_port`: a `bind(0)` port let go is there for any other test's
/// `bind(0)` to take before the engine binds it, and a wiremock `/health`
/// on it answers the registry's poll as if it were the engine (the
/// listener-ownership check is not built yet, WP3).
///
/// [`free_port`]: crate::support::device_link::free_port
fn registry(launcher: Arc<dyn Launcher>, ports: Vec<u16>) -> Arc<Registry> {
    let queue = Arc::new(Mutex::new(VecDeque::from(ports)));
    Arc::new(Registry::with_launcher(
        launcher,
        Arc::new(NoPodman),
        reqwest::Client::new(),
        Arc::new(move || {
            Ok(queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(crate::support::device_link::free_port))
        }),
    ))
}

/// A chat row whose freeform args reach the fake engine as written.
fn runtime(model_id: &str, args: &[&str]) -> ModelRuntime {
    ModelRuntime {
        class: Class::Chat,
        model_id: model_id.into(),
        image: "unused-by-the-native-launcher".into(),
        extra_run_args: vec![],
        idle_seconds: 0,
        warm_start: false,
        enabled: true,
        llama: Some(LlamaArgs::Chat {
            gguf_path: format!("{model_id}.gguf"),
            params: Box::default(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }),
        audio: None,
        audio_settings: None,
        image_model: None,
        sdcpp_caps: None,
        rung: None,
    }
}

/// The engine's PID, through the registry's attribution read.
async fn pid_of(reg: &Registry, name: &str) -> Option<u32> {
    reg.host_pids(&[name.to_string()])
        .await
        .unwrap()
        .get(name)
        .copied()
}

/// Whether a process with `pid` runs: it can be signalled, and on Linux it
/// is no zombie waiting for whoever inherited it.
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the pid can be signalled.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        let state = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start());
        return !state.is_some_and(|s| s.starts_with('Z'));
    }
    true
}

/// Windows: whether the process `pid` has not exited.
#[cfg(windows)]
fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: plain kernel calls; the handle is closed once.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

async fn gone_within(pid: u32, bound: Duration) -> bool {
    let until = Instant::now() + bound;
    while Instant::now() < until {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !alive(pid)
}

#[tokio::test]
async fn a_native_engine_starts_serves_logs_and_stops() {
    let rig = Rig::new(vec![]);
    let models = rig.models();
    let rt = runtime("fake-chat", &["--fake-load-ms", "600"]);
    let name = container_name("lmgw", Class::Chat, "fake-chat");

    let guard = rig
        .reg
        .acquire(&rig.spec(&models, &rt, 30_000))
        .await
        .unwrap();
    let view = rig.reg.list();
    assert_eq!(view[0].state, RuntimeState::Ready);
    let port = view[0].port;
    assert_eq!(guard.endpoint(), format!("http://127.0.0.1:{port}"));
    let pid = pid_of(&rig.reg, &name)
        .await
        .expect("a running child has a PID");

    let log = rig.reg.logs_tail(&name, 200).await.unwrap();
    // The host's view: the models dir on disk, the port on the loopback.
    assert!(
        log.contains(&format!("-m {models}/fake-chat.gguf")),
        "{log}"
    );
    assert!(
        log.contains(&format!("--host 127.0.0.1 --port {port}")),
        "{log}"
    );
    assert!(log.contains(&format!("fake engine: pid {pid}")), "{log}");
    // No run args name a card: every GPU backend is hidden.
    assert!(log.contains("env CUDA_VISIBLE_DEVICES=-1"), "{log}");
    assert!(log.contains("env GGML_VK_VISIBLE_DEVICES="), "{log}");
    // The engine runs from its install dir (compared by its last component:
    // the temp dir's spelling differs between a canonical path and the
    // child's own, a symlinked /tmp or Windows' `\\?\` prefix).
    let cwd = log
        .lines()
        .find_map(|l| l.strip_prefix("fake engine: cwd "))
        .unwrap_or_else(|| panic!("no cwd line: {log}"));
    assert_eq!(
        Path::new(cwd).file_name(),
        rig.build_dir().file_name(),
        "{log}"
    );
    // The readiness poll is in the log, a 503 while it loaded included.
    assert!(log.contains("request GET /health"), "{log}");
    assert_eq!(
        rig.reg.logs_tail(&name, 1).await.unwrap().lines().count(),
        1
    );

    drop(guard);
    rig.reg.stop(Class::Chat, "fake-chat", false).await.unwrap();
    assert!(rig.reg.list().is_empty());
    assert_eq!(pid_of(&rig.reg, &name).await, None);
    assert!(!alive(pid), "the stop waited for the exit");
    // A stopped engine keeps its log until the next start under its name.
    assert!(rig
        .reg
        .logs_tail(&name, 200)
        .await
        .unwrap()
        .contains("listening on"));

    // Delete hygiene forgets it, log included.
    rig.reg
        .stop_and_remove("lmgw", Class::Chat, "fake-chat", false)
        .await
        .unwrap();
    assert!(rig.reg.logs_tail(&name, 10).await.is_err());
}

#[tokio::test]
async fn an_engine_that_exits_while_loading_fails_the_start_with_its_exit_and_log() {
    let rig = Rig::new(vec![]);
    let models = rig.models();
    let rt = runtime("dies", &["--fake-exit", "3"]);

    let started = Instant::now();
    let err = rig
        .reg
        .acquire(&rig.spec(&models, &rt, 60_000))
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the exit ended the poll, not the load timeout"
    );
    match err {
        RuntimeError::Start { message, logs, .. } => {
            assert!(message.contains("exited with code 3"), "{message}");
            assert!(message.contains("llama-server"), "{message}");
            assert!(logs.iter().any(|l| l.contains("told to fail")), "{logs:?}");
        }
        other => panic!("expected a start error, got {other:?}"),
    }
    assert!(rig.reg.list().is_empty());
}

#[tokio::test]
async fn a_port_taken_before_the_engine_binds_it_is_retried_once_on_a_fresh_one() {
    // Somebody else holds the first port the allocator hands out: the engine
    // starts, cannot bind, and exits, which is the only way a native engine
    // can say so.
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squatter.local_addr().unwrap().port();
    let rig = Rig::new(vec![taken]);
    let models = rig.models();
    let rt = runtime("rebound", &[]);
    let name = container_name("lmgw", Class::Chat, "rebound");

    rig.reg
        .acquire(&rig.spec(&models, &rt, 60_000))
        .await
        .unwrap();
    let port = rig.reg.list()[0].port;
    assert_ne!(port, taken, "the retry took a fresh port");
    let log = rig.reg.logs_tail(&name, 200).await.unwrap();
    assert!(log.contains(&format!("--port {port}")), "{log}");
    assert!(
        !log.contains("couldn't bind"),
        "the first attempt's process and log were collected: {log}"
    );
    drop(squatter);
}

/// Linux: the spawner thread's death signal and the supervisors' kill;
/// Windows: the supervisors' kill or the kill-on-close job's handle closing
/// with the launcher, whichever comes first. The job on its own is what
/// [`a_killed_lmgw_takes_its_engines_with_it`] asserts there.
#[tokio::test]
async fn a_dropped_launcher_takes_its_engines_with_it() {
    let rig = Rig::new(vec![]);
    let models = rig.models();
    let rt = runtime("tied", &[]);
    let name = container_name("lmgw", Class::Chat, "tied");
    let guard = rig
        .reg
        .acquire(&rig.spec(&models, &rt, 30_000))
        .await
        .unwrap();
    let pid = pid_of(&rig.reg, &name).await.unwrap();
    assert!(alive(pid));

    drop(guard);
    let Rig { dir, reg } = rig;
    drop(reg);
    assert!(
        gone_within(pid, Duration::from_secs(10)).await,
        "the engine outlived its launcher"
    );
    drop(dir);
}

/// The env var that turns [`lmgw_dies_helper`] from a no-op into the
/// "lmgw" of [`a_killed_lmgw_takes_its_engines_with_it`]: the file it writes
/// the engine's PID into.
#[cfg(any(target_os = "linux", windows))]
const HELPER_PID_FILE: &str = "LMGW_TEST_NATIVE_HELPER_PID_FILE";

/// Not a test on its own: when [`HELPER_PID_FILE`] is set, this process
/// plays lmgw — it starts the fake engine through the native launcher,
/// writes the engine's PID, and waits to be killed.
#[cfg(any(target_os = "linux", windows))]
#[tokio::test]
async fn lmgw_dies_helper() {
    let Some(out) = std::env::var_os(HELPER_PID_FILE) else {
        return;
    };
    // Inside the calling test's own temp dir, which outlives this killed
    // process and is removed by that test.
    let parent = PathBuf::from(&out).parent().unwrap().to_path_buf();
    let rig = Rig::new_in(tempfile::tempdir_in(parent).unwrap(), vec![]);
    let models = rig.models();
    let rt = runtime("orphan", &[]);
    let name = container_name("lmgw", Class::Chat, "orphan");
    let _guard = rig
        .reg
        .acquire(&rig.spec(&models, &rt, 30_000))
        .await
        .unwrap();
    let pid = pid_of(&rig.reg, &name).await.unwrap();
    std::fs::write(PathBuf::from(&out).with_extension("tmp"), pid.to_string()).unwrap();
    std::fs::rename(PathBuf::from(&out).with_extension("tmp"), &out).unwrap();
    tokio::time::sleep(Duration::from_secs(120)).await;
}

/// lmgw killed outright — no drop, no supervisor, no `kill_on_drop` — still
/// takes its engines with it: on Linux `PR_SET_PDEATHSIG(SIGKILL)` from the
/// spawner thread; on Windows the kill-on-close job, whose last handle goes
/// with the killed process (`TerminateProcess` runs no code of its own). The
/// "lmgw" is this test binary re-run as [`lmgw_dies_helper`].
#[cfg(any(target_os = "linux", windows))]
#[tokio::test]
async fn a_killed_lmgw_takes_its_engines_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("engine.pid");
    let mut lmgw = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime_launcher_native::lmgw_dies_helper",
            "--nocapture",
        ])
        .env(HELPER_PID_FILE, &pid_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(60);
    let engine: u32 = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            break text.trim().parse().unwrap();
        }
        if let Some(status) = lmgw.try_wait().unwrap() {
            panic!("the helper ended ({status}) before it started its engine");
        }
        assert!(
            Instant::now() < until,
            "the helper never started its engine"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(alive(engine));

    lmgw.kill().unwrap();
    lmgw.wait().unwrap();
    // Linux: the engine was the helper's child, so init reaps it once it is
    // killed. Windows: the job's kill-on-close.
    assert!(
        gone_within(engine, Duration::from_secs(10)).await,
        "the engine outlived the lmgw that started it"
    );
}

/// A real llama-server release build, on the CPU, through the native
/// launcher. Skips unless both are set:
///
/// - `LMGW_LIVE_NATIVE_ENGINES`: an engines dir holding exactly one llama
///   build, `<dir>/llama/<build>/llama-server` (Linux: unpack
///   `llama-b12236-bin-ubuntu-x64.tar.gz` into `<dir>/llama/`; Windows: unpack
///   `llama-b12236-bin-win-cpu-x64.zip` into `<dir>\llama\b12236-cpu\`);
/// - `LMGW_LIVE_NATIVE_GGUF`: a small chat GGUF (Qwen3.5-0.8B UD-Q4_K_XL was
///   used for the record in the Windows design §4.1).
///
/// Run: `cargo nextest run --workspace -E 'test(native_live)' --no-capture`.
#[tokio::test]
async fn native_live_llama_server_on_the_cpu_answers_a_chat() {
    let (Some(engines), Some(gguf)) = (
        std::env::var_os("LMGW_LIVE_NATIVE_ENGINES"),
        std::env::var_os("LMGW_LIVE_NATIVE_GGUF"),
    ) else {
        eprintln!("skipped: LMGW_LIVE_NATIVE_ENGINES and LMGW_LIVE_NATIVE_GGUF are not set");
        return;
    };
    let gguf = PathBuf::from(gguf);
    let models = gguf.parent().unwrap().display().to_string();
    let file = gguf.file_name().unwrap().to_string_lossy().into_owned();
    let native = Native::with_engines(Engines::hand_installed(PathBuf::from(engines))).unwrap();
    let reg = registry(Arc::new(native), vec![]);
    let data = tempfile::tempdir().unwrap();
    let rt = ModelRuntime {
        llama: Some(LlamaArgs::Chat {
            gguf_path: file,
            params: Box::new(LlamaParams {
                n_gpu_layers: Some(0),
                ctx_size: Some(4096),
                jinja: true,
                ..LlamaParams::default()
            }),
            args: vec![],
        }),
        ..runtime("live-chat", &[])
    };
    let spec = AcquireSpec {
        runtime: &rt,
        container_prefix: "lmgw",
        models_dir: &models,
        data_dir: data.path(),
        may_write_models_dir: false,
        load_timeout: Duration::from_secs(600),
        stop_timeout: Duration::from_secs(30),
    };
    let name = container_name("lmgw", Class::Chat, "live-chat");

    let t0 = Instant::now();
    let guard = match reg.acquire(&spec).await {
        Ok(g) => g,
        Err(e) => panic!(
            "{e}\n--- log ---\n{}",
            reg.logs_tail(&name, 80).await.unwrap_or_default()
        ),
    };
    let ready = t0.elapsed();
    let t1 = Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", guard.endpoint()))
        .json(&serde_json::json!({
            "messages": [{"role": "user", "content": "Name three colors."}],
            "max_tokens": 32,
            "chat_template_kwargs": {"enable_thinking": false}
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap();
    let request = t1.elapsed();
    assert_eq!(status, 200, "{body}");
    let text = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!text.trim().is_empty(), "{body}");
    let pid = pid_of(&reg, &name).await.unwrap();

    drop(guard);
    let t2 = Instant::now();
    reg.stop(Class::Chat, "live-chat", false).await.unwrap();
    let stopped = t2.elapsed();
    assert_eq!(pid_of(&reg, &name).await, None);
    eprintln!(
        "native live: pid {pid}, ready in {ready:?}, chat in {request:?}, stopped in \
         {stopped:?}; answer: {}",
        text.trim().replace('\n', " ")
    );
}

/// Windows: the run args' `--memory` is a job memory limit of the engine's
/// own (`launcher::native::limits`), so a commit past it fails inside the
/// engine, and `--cpus` that job's CPU rate hard cap, as the engine reads
/// them from its own job; the same engine without them has neither and
/// commits as much. Every start here was created suspended and ran only once
/// in the job.
#[cfg(windows)]
#[tokio::test]
async fn a_memory_limit_in_the_run_args_holds_the_native_engine() {
    let rig = Rig::new(vec![]);
    let models = rig.models();
    let mut capped = runtime("capped", &["--fake-alloc-mib", "256"]);
    capped.extra_run_args = vec!["--memory".into(), "64m".into(), "--cpus=1".into()];
    let _a = rig
        .reg
        .acquire(&rig.spec(&models, &capped, 30_000))
        .await
        .unwrap();
    let log = rig
        .reg
        .logs_tail(&container_name("lmgw", Class::Chat, "capped"), 200)
        .await
        .unwrap();
    assert!(log.contains("could not allocate 256 MiB"), "{log}");
    assert!(
        log.contains("fake engine: job memory limit 67108864"),
        "{log}"
    );
    // `--cpus=1` of every processor group's logical processors.
    match lmgw_core::runtime::launcher::native::limits::cpu_rate(1.0, lmgw_machine::cpu::logical())
    {
        Some(rate) => assert!(
            log.contains(&format!("fake engine: job cpu rate {rate} hard cap")),
            "{log}"
        ),
        None => assert!(log.contains("fake engine: job cpu rate none"), "{log}"),
    }

    let free = runtime("uncapped", &["--fake-alloc-mib", "256"]);
    let _b = rig
        .reg
        .acquire(&rig.spec(&models, &free, 30_000))
        .await
        .unwrap();
    let log = rig
        .reg
        .logs_tail(&container_name("lmgw", Class::Chat, "uncapped"), 200)
        .await
        .unwrap();
    assert!(log.contains("fake engine: allocated 256 MiB"), "{log}");
    assert!(log.contains("fake engine: job memory limit none"), "{log}");
    assert!(log.contains("fake engine: job cpu rate none"), "{log}");
}
