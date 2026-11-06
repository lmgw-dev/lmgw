//! Which rows a machine check's problem concerns, and who is never swapped
//! for it (machine-check design §2.5, D64, D95–D98, WP5): §1.1 is scoped
//! per row as §1.2 is — a row on the render nodes alone (a Vulkan engine)
//! is not blamed for a card it does not use — a pinned caller tries the
//! start rather than take a fallback it would refuse, a warm says the
//! fallback answers, a failed write of lmgw's own spec is swapped for only
//! when the swap can tell it is still so and "Check again" writes it again,
//! and a slow read after one failed start keeps no other model waiting.
//!
//! Over HTTP on `support/gpu_world.rs` (and the admission fixture for the
//! pinned embedder), the machine stated by `support/machine_world.rs`.

use std::time::{Duration, Instant};

use lmgw_api_types::machine::{MachineCheck, MachineVerdict};
use lmgw_core::gate::RouteCheck;
use lmgw_core::runtime::registry::Origin;
use lmgw_core::runtime::Class;
use lmgw_core::vram::Target;
use lmgw_machine::CmdOut;

use super::gpu_setup::{
    ask, driver_updated, from_the_cloud, h, not_ready, verdict, world, RESTART,
};
use super::*;
use crate::support::device_pair::op;
use crate::support::fallbacks::set_fallback;
use crate::support::gpu_world::Gpu;
use crate::support::machine_world::{Machine, World as MachineWorld};

/// A chat row whose container is given the render nodes alone: a Vulkan
/// engine.
fn vulkan(id: &str) -> NewLocalModel {
    NewLocalModel {
        model_id: id.into(),
        gguf_path: format!("{id}.gguf"),
        params: Default::default(),
        args: vec![],
        idle_seconds: 0,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: Some(vec!["--device".into(), "/dev/dri".into()]),
        warm_start: false,
        fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

/// What the last check names for a chat row's start, as `gpu_setup` says
/// it after the name, when the resolve-time swap takes it.
fn swapped_for(g: &Gpu, id: &str) -> Option<String> {
    lmgw_core::machine::problem_at_resolve(
        &g.state,
        &g.state.snapshot(),
        &Target {
            class: Class::Chat,
            model_id: id.into(),
        },
    )
}

/// A world on `machine` with rows `m1` (the NVIDIA card) and the Vulkan
/// rows `vk` and `vk2`, each with the cloud alias as its fallback, `vk2`'s
/// start failing; checked once.
async fn vulkan_world(machine: &Machine) -> (Gpu, Gw, MachineCheck) {
    let g = Gpu::new(24 * GIB, 6, 2).await;
    machine.state_on(&g.state);
    g.model("m1", 8 * GIB).await;
    g.row(vulkan("vk"), 4 * GIB).await;
    g.row(vulkan("vk2"), 4 * GIB).await;
    g.cloud("cloud", None).await;
    for id in ["m1", "vk", "vk2"] {
        set_fallback(&g.state, "local_models", id, Some("cloud")).await;
    }
    g.world().fail_run.insert("vk2".into());
    let c = (*lmgw_core::machine::check(&g.state, true).await).clone();
    let gw = common::serve(g.state.clone()).await;
    (g, gw, c)
}

/// The Vulkan rows on an Intel card's render nodes: never swapped, a start
/// that works answers and one that fails keeps its own error; the NVIDIA
/// row is blamed, in the words for no card at all.
#[tokio::test]
async fn an_intel_card_blames_only_the_row_given_the_nvidia_card() {
    let machine = Machine::nvidia_at("600.10.01");
    let f = &machine.m.facts;
    f.remove("/sys/bus/pci/devices");
    MachineWorld::pci(f, "0000:00:02.0", 0x8086, 0xa780, Some("i915"));
    f.file("/dev/dri/renderD128", b"");
    let (g, gw, c) = vulkan_world(&machine).await;
    assert_eq!(c.verdict, MachineVerdict::CloudOnly, "{c:#?}");

    assert_eq!(swapped_for(&g, "vk"), None);
    let resp = ask(&gw, "vk", false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(h(&resp, "x-lmgw-fallback"), None);
    let resp = ask(&gw, "vk2", false).await;
    assert_eq!(resp.status(), 502, "its own error, not handed on");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream", "{body}");

    set_fallback(&g.state, "local_models", "m1", None).await;
    g.world().fail_run.insert("m1".into());
    not_ready(
        ask(&gw, "m1", false).await,
        "'m1' could not start: no graphics card was found on the computer lmgw runs on. Cloud \
         models still work.",
        &["its start said:"],
    )
    .await;
    assert_eq!(g.runs(), ["vk", "vk2", "m1"], "each start tried");
}

/// An NVIDIA card whose driver waits for a restart beside an AMD card: the
/// NVIDIA row is swapped, the Vulkan rows on the render nodes are not, and
/// a Vulkan start that fails keeps its own error.
#[tokio::test]
async fn a_broken_nvidia_driver_beside_an_amd_card_leaves_the_vulkan_rows_alone() {
    let machine = Machine::nvidia_at("600.10.01");
    let f = &machine.m.facts;
    MachineWorld::pci(f, "0000:c5:00.0", 0x1002, 0x15bf, Some("amdgpu"));
    f.file("/dev/kfd", b"").file("/dev/dri/renderD128", b"");
    f.set_driver_libraries("600.20.03");
    let (g, gw, c) = vulkan_world(&machine).await;
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart, "{c:#?}");

    assert!(swapped_for(&g, "m1").is_some());
    assert_eq!(swapped_for(&g, "vk"), None);
    assert_eq!(ask(&gw, "vk", false).await.status(), 200);
    let resp = ask(&gw, "vk2", false).await;
    assert_eq!(resp.status(), 502);
    assert_eq!(h(&resp, "x-lmgw-fallback"), None);
    from_the_cloud(ask(&gw, "m1", false).await, "gpu_setup").await;
    assert_eq!(g.runs(), ["vk", "vk2"], "the NVIDIA row was swapped");
}

/// A pinned caller (quickdoc's embedder, `gate::open_pinned`) is not
/// swapped for the check's verdict: its start is tried, and its answer is
/// `gpu_setup` — never the fallback it would refuse, nor a refusal that
/// names a hold that is not on. A request that may fall back is swapped.
#[tokio::test]
async fn a_pinned_caller_tries_the_start_and_is_told_gpu_setup() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    let machine = Machine::nvidia_at("600.10.01");
    machine.state_on(&f.state);
    let cloud = cloud_upstream(&f, "cloud-embed").await;
    set_fallback(&f.state, "aux_models", "embed-model", Some("cloud-embed")).await;
    machine.m.facts.set_driver_libraries("600.20.03");
    let c = lmgw_core::machine::check(&f.state, true).await;
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart);
    let target = Target {
        class: Class::Aux,
        model_id: "embed-model".into(),
    };
    assert!(
        lmgw_core::machine::problem_at_resolve(&f.state, &f.state.snapshot(), &target).is_some(),
        "a request that may fall back is swapped"
    );
    f.world().fail_run_file = Some("embed-model.gguf".into());

    let failed = lmgw_core::gate::open_pinned(
        &f.state,
        "embed/embed-model",
        RouteCheck::Embeddings,
        Origin::Owner,
    )
    .await
    .unwrap_err();
    assert_eq!(failed.error.kind(), "gpu_setup", "{}", failed.error);
    assert_eq!(failed.headers.fallback(), None);
    let msg = failed.error.to_string();
    assert!(
        msg.starts_with(
            "'embed-model' could not start on the graphics card of the computer lmgw runs on. \
             Restart the computer"
        ),
        "{msg}"
    );
    assert!(!msg.contains("hold"), "{msg}");
    let tried = |f: &Fixture| {
        f.world()
            .run_files
            .iter()
            .filter(|(m, ..)| m == "embed-model")
            .count()
    };
    assert_eq!(tried(&f), 1, "the start was tried");

    let err = InProcessEmbedder::probe(f.state.clone(), "embed/embed-model")
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("could not start on the graphics card"),
        "{msg}"
    );
    assert!(!msg.contains("hold"), "{msg}");
    assert_eq!(tried(&f), 2);
    assert_eq!(
        cloud.received_requests().await.unwrap().len(),
        0,
        "nothing bought from the fallback"
    );

    let resp = embed(&f.gateway).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(fallback_reason(&resp), Some("gpu_setup"));
    assert_eq!(tried(&f), 2, "swapped at resolve");
}

/// A Background warm (a session's, a read-aloud's) of a stage this
/// computer cannot start says its fallback answers: after a start that
/// failed and was blamed, and, once the check says so, without a start.
/// Without a fallback it failed, in the check's words.
#[tokio::test]
async fn a_warm_of_a_stage_this_computer_cannot_start_says_its_fallback() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, _gw) = world(&machine).await;
    g.cloud("cloud", None).await;
    set_fallback(&g.state, "local_models", "m1", Some("cloud")).await;
    driver_updated(&g, &machine);
    let warm = || lmgw_core::realtime::background_warm_for_tests(&g.state, "chat", "m1");

    let frame = warm().await;
    assert_eq!(frame["state"], "fallback", "{frame}");
    assert_eq!(frame["answered_by"], "cloud", "{frame}");
    assert_eq!(g.runs(), ["m1"], "tried, and blamed");

    g.state.machine.settled().await;
    let frame = warm().await;
    assert_eq!(frame["state"], "fallback", "{frame}");
    assert_eq!(g.runs(), ["m1"], "nothing started");

    set_fallback(&g.state, "local_models", "m1", None).await;
    let frame = warm().await;
    assert_eq!(frame["state"], "failed", "{frame}");
    let msg = frame["message"].as_str().unwrap();
    assert!(msg.contains(&RESTART["'m1' ".len()..]), "{msg}");
}

/// A gateway that writes lmgw's own spec (`cdi_machine`), with row `m1`
/// whose fallback is the cloud alias; checked once: ready.
async fn managed(machine: &Machine) -> (Gpu, Gw, tempfile::TempDir) {
    let g = Gpu::new(24 * GIB, 6, 2).await;
    machine.state_on(&g.state);
    let dir = tempfile::Builder::new()
        .prefix("lmgw-gpu-setup-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    g.state.set_cdi_for_tests(machine.m.own_spec(dir.path()));
    g.model("m1", 8 * GIB).await;
    g.cloud("cloud", None).await;
    set_fallback(&g.state, "local_models", "m1", Some("cloud")).await;
    let c = lmgw_core::machine::check(&g.state, true).await;
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
    let gw = common::serve(g.state.clone()).await;
    (g, gw, dir)
}

/// A folder of lmgw's that others may write into: blamed, answered by the
/// fallback, and never swapped for — nothing the swap reads changes when
/// it is fixed — so the first request after the fix starts the model.
#[tokio::test]
async fn a_folder_that_is_not_private_is_not_swapped_for() {
    use std::os::unix::fs::PermissionsExt;
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw, _dir) = managed(&machine).await;
    let folder = machine.m.runtime_dir.path().join("lmgw-machine");
    std::fs::create_dir(&folder).unwrap();
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o777)).unwrap();

    from_the_cloud(ask(&gw, "m1", false).await, "gpu_setup").await;
    g.state.machine.settled().await;
    assert_eq!(verdict(&g), MachineVerdict::NeedsSteps);
    assert_eq!(swapped_for(&g, "m1"), None);
    let reads = g.state.machine.blame_reads();
    from_the_cloud(ask(&gw, "m1", false).await, "gpu_setup").await;
    assert_eq!(
        g.state.machine.blame_reads(),
        reads,
        "tried, and answered by the last check: the machine is as it read it"
    );

    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
    let resp = ask(&gw, "m1", false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(h(&resp, "x-lmgw-fallback"), None);
    assert_eq!(g.runs(), ["m1"]);
    g.state.machine.settled().await;
    assert_eq!(verdict(&g), MachineVerdict::Ready);
}

/// The toolkit's write failing: swapped for while `nvidia-ctk` is the file
/// the check read, also once the toolkit works again — until "Check again"
/// writes lmgw's spec again first, and the next request starts the model.
#[tokio::test]
async fn a_failed_write_is_swapped_for_until_check_again_writes_it() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw, _dir) = managed(&machine).await;
    machine
        .m
        .fail_with(Some(CmdOut::failure(1, "level=error msg=\"boom\"\n")));

    from_the_cloud(ask(&gw, "m1", false).await, "gpu_setup").await;
    g.state.machine.settled().await;
    assert!(swapped_for(&g, "m1").is_some());
    let writes = machine.m.generate_calls().len();
    from_the_cloud(ask(&gw, "m1", false).await, "gpu_setup").await;
    assert_eq!(
        machine.m.generate_calls().len(),
        writes,
        "swapped: no start"
    );

    machine.m.fail_with(None);
    assert!(
        swapped_for(&g, "m1").is_some(),
        "nothing the swap reads changed"
    );
    let (status, body) = op(&gw, "machine_check", json!({ "fresh": true })).await;
    assert_eq!(status, 200, "{body}");
    let c: MachineCheck = serde_json::from_value(body).unwrap();
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
    assert_eq!(
        machine.m.generate_calls().len(),
        writes + 1,
        "written again"
    );
    assert_eq!(swapped_for(&g, "m1"), None);
    let resp = ask(&gw, "m1", false).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(h(&resp, "x-lmgw-fallback"), None);
}

/// One start's slow read of the card (NVML in its separate process) keeps
/// no other model waiting: `m2` is admitted and answers while `m1`'s answer
/// still waits for its read.
#[tokio::test]
async fn a_slow_read_after_a_failed_start_keeps_no_other_model_waiting() {
    let machine = Machine::nvidia_at("600.10.01");
    let (g, gw) = world(&machine).await;
    // A new boot since the check, the driver's halves agreeing: the read
    // after a failed start asks NVML.
    machine
        .m
        .facts
        .set_boot_id("0f6c1e9a-5b7d-4e2a-9c3f-000000000003");
    *machine.probe_delay.lock().unwrap() = Duration::from_millis(1500);
    let (tx, rx) = std::sync::mpsc::channel();
    *machine.probe_started.lock().unwrap() = Some(tx);
    g.world().fail_run.insert("m1".into());

    let gw1 = gw.clone();
    let failing = tokio::spawn(async move { ask(&gw1, "m1", false).await.status() });
    tokio::task::spawn_blocking(move || rx.recv().unwrap())
        .await
        .unwrap();
    let asked = Instant::now();
    assert_eq!(ask(&gw, "m2", false).await.status(), 200);
    assert!(
        asked.elapsed() < Duration::from_millis(1500),
        "{:?}",
        asked.elapsed()
    );
    assert!(
        !failing.is_finished(),
        "m1's answer still waits for its read"
    );
    assert_eq!(failing.await.unwrap(), 502, "nothing wrong: its own error");
}
