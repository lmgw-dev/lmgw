//! The run-time refusal (machine-check design §3.6.4): a podman call that
//! gives a container the graphics card (`nvidia.com/gpu`, `--gpus`) without
//! lmgw's `--cdi-spec-dir` before its subcommand is refused where podman
//! runs — the registry's runner, the benchmark's launcher and the agent
//! spawner (an isolated MCP server's create and start ask the same check,
//! `podman_option.rs`) — while lmgw passes its own spec, and let through
//! when Podman's default dirs are in use or no spec is managed.

use std::sync::Arc;

use lmgw_core::agents::container::{CdiChecked, NoSpawner, Spawner};
use lmgw_core::bench::{BenchLauncher, PodmanLauncher};
use lmgw_core::runtime::cdi::OwnSpec;
use lmgw_core::runtime::registry::Registry;

use super::{args, data_dir, gpu_args, Recorder};
use crate::support::cdi_machine::CdiMachine;

/// `podman run … <gpu args> image` without the option.
fn unpointed() -> Vec<String> {
    let mut argv = args(&["run", "--rm"]);
    argv.extend(gpu_args());
    argv.push("img".into());
    argv
}

#[test]
fn the_check_refuses_the_card_without_the_option_and_nothing_else() {
    let m = CdiMachine::new();
    let data = data_dir();
    let own = m.own_spec(data.path());
    let dir = m.spec_dir("lmgw").display().to_string();

    let why = own.refuse_unpointed(&unpointed()).unwrap_err();
    assert!(
        why.starts_with("lmgw did not run `podman run --rm --device nvidia.com/gpu=all")
            && why.contains("--cdi-spec-dir")
            && why.contains("a bug in lmgw"),
        "{why}"
    );
    assert!(own
        .refuse_unpointed(&args(&["run", "--gpus=all", "img"]))
        .is_err());

    let mut pointed = args(&["--cdi-spec-dir", &dir]);
    pointed.extend(unpointed());
    own.refuse_unpointed(&pointed).unwrap();
    own.refuse_unpointed(&args(&["run", "--cpus", "4", "img"]))
        .unwrap();
    own.refuse_unpointed(&args(&["rm", "-f", "lmgw-chat-qwen"]))
        .unwrap();
    // The container's own command after the image is not an option.
    own.refuse_unpointed(&args(&["run", "--rm", "img", "serve", "--gpus", "all"]))
        .unwrap();
    own.refuse_unpointed(&args(&["exec", "c", "--device", "nvidia.com/gpu=all"]))
        .unwrap();

    // Podman's default dirs in use (no toolkit): nothing to point at.
    m.facts.uninstall("/usr/bin/nvidia-ctk");
    own.refuse_unpointed(&unpointed()).unwrap();
    // No spec managed (a test state).
    OwnSpec::unmanaged("a test")
        .refuse_unpointed(&unpointed())
        .unwrap();
}

/// What follows the image is the container's own command: a throwaway whose
/// own arguments say `--device nvidia.com/gpu=all` gives the container
/// nothing, so it runs. (The registry refuses a call whose options name the
/// card without the option, as the check above does; its own start paths
/// take the option from the same run args.)
#[tokio::test]
async fn a_container_argument_that_looks_like_a_device_flag_is_no_device() {
    let m = CdiMachine::new();
    let data = data_dir();
    let podman = Arc::new(Recorder::default());
    let reg = Registry::new(podman.clone(), reqwest::Client::new());
    reg.set_cdi(m.own_spec(data.path()));
    reg.run_throwaway(
        "lmgw",
        "lmgw-probe",
        "img",
        "sh",
        &args(&["--cpus", "4"]),
        &args(&["--device", "nvidia.com/gpu=all", "--gpus", "all"]),
    )
    .await
    .unwrap();
    assert_eq!(podman.calls().len(), 1, "{:?}", podman.calls());
}

#[tokio::test]
async fn the_bench_launcher_refuses_before_podman_runs() {
    let m = CdiMachine::new();
    let data = data_dir();
    let podman = Arc::new(Recorder::default());
    let launcher = PodmanLauncher::new(podman.clone(), m.own_spec(data.path()));
    let err = launcher.podman(&unpointed()).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(podman.calls().is_empty());
    launcher
        .podman(&args(&["rm", "-f", "lmgw-bench-1"]))
        .await
        .unwrap();
    assert_eq!(podman.calls().len(), 1);
}

#[tokio::test]
async fn the_agent_spawner_refuses_before_podman_runs() {
    let m = CdiMachine::new();
    let data = data_dir();
    // The inner spawner refuses everything with its own words: a refusal in
    // lmgw's words is the check's.
    let checked = CdiChecked {
        inner: Arc::new(NoSpawner("the inner spawner was reached".into())),
        cdi: m.own_spec(data.path()),
    };
    for err in [
        checked.run("podman", &unpointed()).await.unwrap_err(),
        checked
            .spawn("podman", &unpointed())
            .await
            .err()
            .expect("refused"),
        checked
            .spawn_env("podman", &unpointed(), &[])
            .await
            .err()
            .expect("refused"),
    ] {
        assert!(err.to_string().contains("lmgw did not run"), "{err}");
    }
    let through = checked
        .run("podman", &args(&["ps", "-a"]))
        .await
        .unwrap_err();
    assert!(through
        .to_string()
        .contains("the inner spawner was reached"));
}

/// What the gateway hands out is checked whatever spawner is installed.
#[tokio::test]
async fn the_gateways_agent_spawner_is_the_checked_one() {
    let m = CdiMachine::new();
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    state.set_cdi_for_tests(m.own_spec(&state.data_dir));
    let err = state
        .agent_spawner()
        .run("podman", &unpointed())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("lmgw did not run"), "{err}");
}
