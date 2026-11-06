//! The guard of machine-check design §3.6.4: every builder that renders a
//! podman command carrying a class's or a row's run arguments puts lmgw's
//! `--cdi-spec-dir` before the subcommand when those arguments name an NVIDIA
//! device, and no option when they do not. The builders: the container argv
//! and its copyable command line (`runtime/argv.rs`), the model start
//! (`registry/acquire.rs`), the throwaway probes (`registry/podman_ops.rs`:
//! the `--help` reads), the command line a row shows (`ops`), the builds'
//! GPU verify (`backends/run/verify.rs`), an isolated MCP server's create
//! and start (`mcp/container.rs`, `mcp/connect.rs`) — and the benchmark's
//! container, whose guard is `bench/cdi.rs`, beside the world it runs in.
//!
//! A podman call written by hand elsewhere is not covered here. Two things
//! catch one: `refusal.rs`, the run-time refusal where podman runs, and
//! `podman_sites.rs`, the list every podman spawn site in the source has to
//! be on.

use std::path::Path;
use std::sync::Arc;

use lmgw_core::config::{LlamaParams, McpServer, McpTransport};
use lmgw_core::runtime::argv::{command_line, podman_run_argv, stamp_owner, LlamaArgs};
use lmgw_core::runtime::cdi::SPEC_DIR_OPTION;
use lmgw_core::runtime::descriptor::ModelRuntime;
use lmgw_core::runtime::registry::{AcquireSpec, Registry};
use lmgw_core::state::AppState;
use lmgw_core::store::{self, NewLocalModel};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{args, data_dir, gpu_args, Recorder};
use crate::support::cdi_machine::CdiMachine;

/// A chat row on the official image with `extra_run_args`.
pub(super) fn chat_runtime(model_id: &str, extra_run_args: Vec<String>) -> ModelRuntime {
    ModelRuntime {
        class: lmgw_core::runtime::Class::Chat,
        model_id: model_id.into(),
        image: "ghcr.io/ggml-org/llama.cpp:server-cuda".into(),
        extra_run_args,
        idle_seconds: 0,
        warm_start: false,
        enabled: true,
        llama: Some(LlamaArgs::Chat {
            gguf_path: format!("{model_id}.gguf"),
            params: Box::<LlamaParams>::default(),
            args: vec![],
        }),
        audio: None,
        audio_settings: None,
        image_model: None,
        sdcpp_caps: None,
        rung: None,
    }
}

/// CPU run args: no device at all.
fn cpu_args() -> Vec<String> {
    args(&["-e", "CUDA_VISIBLE_DEVICES=", "--cpus", "4"])
}

/// `argv` starts with lmgw's spec dir `dir`, then `run`.
#[track_caller]
fn assert_gpu(argv: &[String], dir: &Path) {
    assert_eq!(
        argv[..3],
        [
            SPEC_DIR_OPTION.to_string(),
            dir.display().to_string(),
            "run".to_string()
        ],
        "{argv:?}"
    );
}

/// `argv` starts with `run`: no global option.
#[track_caller]
fn assert_cpu(argv: &[String]) {
    assert_eq!(argv[0], "run", "{argv:?}");
    assert!(!argv.iter().any(|a| a == SPEC_DIR_OPTION), "{argv:?}");
}

#[test]
fn the_container_argv_and_its_command_line_put_the_globals_before_run() {
    let rt = chat_runtime("qwen", gpu_args());
    let mut spec = rt
        .preview_spec("lmgw", 41234, "/srv/models", Path::new("/tmp"))
        .unwrap();
    assert!(
        spec.globals.is_empty(),
        "a renderer leaves them to the caller"
    );
    spec.globals = args(&["--cdi-spec-dir", "/run/user/1000/lmgw-machine/lmgw/cdi"]);
    let argv = stamp_owner(podman_run_argv(&spec), "instance-1");
    assert_gpu(&argv, Path::new("/run/user/1000/lmgw-machine/lmgw/cdi"));
    assert!(argv.contains(&"lmgw.owner=instance-1".to_string()));
    assert!(command_line(&spec).starts_with(
        "podman --cdi-spec-dir /run/user/1000/lmgw-machine/lmgw/cdi run -d --replace"
    ));
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

#[tokio::test]
async fn a_model_start_passes_lmgws_dir_for_a_gpu_row_and_nothing_for_a_cpu_row() {
    let m = CdiMachine::new();
    let data = data_dir();
    let health = healthy().await;
    let port = health.address().port();
    let podman = Arc::new(Recorder::default());
    let reg = Arc::new(Registry::with_ports(
        podman.clone(),
        reqwest::Client::new(),
        Arc::new(move || Ok(port)),
    ));
    reg.set_cdi(m.own_spec(data.path()));
    let spec = |rt| AcquireSpec {
        runtime: rt,
        container_prefix: "lmgw",
        models_dir: "/srv/models",
        data_dir: data.path(),
        may_write_models_dir: true,
        load_timeout: std::time::Duration::from_secs(5),
        stop_timeout: std::time::Duration::from_millis(500),
    };

    let gpu = chat_runtime("gpu-row", gpu_args());
    drop(reg.acquire(&spec(&gpu)).await.unwrap());
    let cpu = chat_runtime("cpu-row", cpu_args());
    drop(reg.acquire(&spec(&cpu)).await.unwrap());

    let runs = podman.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_gpu(&runs[0], &m.spec_dir("lmgw"));
    assert_cpu(&runs[1]);
    assert_eq!(m.generations(), 1, "the GPU start made the spec first");
    assert!(m.spec_dir("lmgw").join("nvidia.json").exists());
}

#[tokio::test]
async fn the_throwaway_probes_pass_it_for_gpu_args_only() {
    let m = CdiMachine::new();
    let data = data_dir();
    let podman = Arc::new(Recorder::default());
    let reg = Arc::new(Registry::new(podman.clone(), reqwest::Client::new()));
    reg.set_cdi(m.own_spec(data.path()));
    let image = "ghcr.io/ggml-org/llama.cpp:server-cuda";

    // The llama `--help` read tries each place the binary may be; every try
    // carries the option.
    let _ = reg.help_text("lmgw", image, &gpu_args()).await;
    let gpu_runs = podman.runs();
    assert!(!gpu_runs.is_empty());
    for argv in &gpu_runs {
        assert_gpu(argv, &m.spec_dir("lmgw"));
    }
    // The sd-server read too.
    let _ = reg.sdcpp_help_text("lmgw", image, &gpu_args()).await;
    let sd = podman
        .runs()
        .into_iter()
        .skip(gpu_runs.len())
        .collect::<Vec<_>>();
    assert!(!sd.is_empty());
    for argv in &sd {
        assert_gpu(argv, &m.spec_dir("lmgw"));
    }

    let before = podman.runs().len();
    let _ = reg.help_text("lmgw", "another:image", &cpu_args()).await;
    for argv in podman.runs().iter().skip(before) {
        assert_cpu(argv);
    }
}

#[tokio::test]
async fn the_command_line_a_row_shows_carries_it() {
    let m = CdiMachine::new();
    let state = AppState::init_for_tests().await.unwrap();
    state.set_cdi_for_tests(m.own_spec(&state.data_dir));
    let prefix = state.snapshot().settings.container_prefix.clone();
    for (id, run_args) in [("gpu-row", None), ("cpu-row", Some(cpu_args()))] {
        store::insert_local_model(
            &state.db,
            &NewLocalModel {
                model_id: id.into(),
                gguf_path: format!("{id}.gguf"),
                params: LlamaParams::default(),
                args: vec![],
                idle_seconds: 0,
                enabled: true,
                public: true,
                image: None,
                extra_run_args: run_args,
                warm_start: false,
                fallback: None,
                capabilities_override: None,
                ladder: vec![],
            },
        )
        .await
        .unwrap();
    }
    state.reload_snapshot().await.unwrap();
    let line = |id: &'static str| {
        let state = state.clone();
        async move {
            let got = crate::common::read_wire(
                lmgw_core::ops::local_model_get(&state, None, Some(id), None)
                    .await
                    .unwrap(),
            );
            got["command_line"].as_str().unwrap().to_string()
        }
    };
    let gpu = line("gpu-row").await;
    let want = format!(
        "podman --cdi-spec-dir {} run -d --replace",
        m.spec_dir(&prefix).display()
    );
    assert!(gpu.starts_with(&want), "{gpu}");
    let cpu = line("cpu-row").await;
    assert!(cpu.starts_with("podman run -d --replace"), "{cpu}");
    // The read also checks the GPU row's flags against the image's `--help`,
    // a throwaway with the GPU args, which made the spec fresh as every GPU
    // probe does: once, the CPU row's read needed nothing.
    assert_eq!(m.generations(), 1);
}

#[tokio::test]
async fn the_builds_gpu_verify_probes_carry_it() {
    use crate::support::backends_fake::Harness;
    use lmgw_core::backends::run;
    use lmgw_core::backends::{BuildRunStatus, BuildTrigger};

    let m = CdiMachine::new();
    let h = Harness::new().await;
    h.state.set_cdi_for_tests(m.own_spec(&h.state.data_dir));
    let prefix = h.state.snapshot().settings.container_prefix.clone();
    let build_id = h.build(&h.spec()).await;
    let started = run::start_run(&h.state, build_id, BuildTrigger::Manual, false)
        .await
        .unwrap();
    let r = crate::support::backends_fake::wait_run(&h.state, started.run_id).await;
    assert_eq!(r.status, BuildRunStatus::Succeeded, "{:?}", r.error);
    let probes: Vec<Vec<String>> = h
        .podman
        .world()
        .calls
        .iter()
        .filter(|c| c.iter().any(|a| a == "--entrypoint"))
        .cloned()
        .collect();
    assert!(probes.len() >= 2, "help and device probes: {probes:?}");
    for argv in &probes {
        assert_gpu(argv, &m.spec_dir(&prefix));
    }
    let log = std::fs::read_to_string(r.log_path.clone().expect("the run's log")).unwrap();
    assert!(
        log.contains(&format!(
            "$ podman --cdi-spec-dir {} run --rm",
            m.spec_dir(&prefix).display()
        )),
        "the verify log shows the option:\n{log}"
    );
}

/// An isolated MCP server whose run args give its container the graphics
/// card: the globals its connect computes come before `create` and `start`
/// (Podman resolves the device when `start` initialises the container), the
/// spec is made fresh first, and both argvs pass the run-time check that the
/// same argv without them fails.
#[tokio::test]
async fn an_isolated_mcp_servers_create_and_start_carry_it() {
    let m = CdiMachine::new();
    let state = AppState::init_for_tests().await.unwrap();
    state.set_cdi_for_tests(m.own_spec(&state.data_dir));
    let prefix = state.snapshot().settings.container_prefix.clone();
    let dir = m.spec_dir(&prefix);
    let mut server = McpServer {
        id: 7,
        name: "gpu-tools".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: None,
        args: vec![],
        env: vec![],
        cwd: None,
        container_image: Some("ghcr.io/acme/mcp-gpu".into()),
        extra_run_args: gpu_args(),
        url: None,
        headers: vec![],
        tool_prefix: "gpu".into(),
        timeout_ms: 30_000,
        autostart: false,
        idle_seconds: 0,
        allow_sampling: false,
        sampling_alias: None,
        agent_id: None,
        device_key_id: None,
    };
    let globals = lmgw_core::mcp::container::podman_globals(&state, &server)
        .await
        .unwrap();
    assert_eq!(m.generations(), 1, "made fresh before the create");
    let create = server.container_create_argv(&globals, &[]).unwrap();
    assert_eq!(
        create[..3],
        [
            SPEC_DIR_OPTION.to_string(),
            dir.display().to_string(),
            "create".to_string()
        ]
    );
    let start = server.container_start_argv(&globals, "c0ffee");
    assert_eq!(
        start[..3],
        [
            SPEC_DIR_OPTION.to_string(),
            dir.display().to_string(),
            "start".to_string()
        ]
    );
    state.cdi().refuse_unpointed(&create).unwrap();
    state.cdi().refuse_unpointed(&start).unwrap();
    let bare_create = server.container_create_argv(&[], &[]).unwrap();
    assert!(state.cdi().refuse_unpointed(&bare_create).is_err());

    // A server without the card gets nothing, and writes nothing.
    server.extra_run_args = cpu_args();
    assert!(lmgw_core::mcp::container::podman_globals(&state, &server)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(m.generations(), 1);
}
