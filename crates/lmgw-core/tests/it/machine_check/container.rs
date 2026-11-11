//! The check in the server image (headless server design §7, §12's
//! "Container mode words"): every §7 row's sentence from stated facts — the
//! card passed in or not, the host's driver, the image's toolkit without a
//! plan, Podman inside the lmgw container as its ID maps show it, each room
//! by its volume — and the same states on the desktop keeping the desktop's
//! words (ruling 7).

use lmgw_api_types::machine::{MachineCause, MachineItemId, MachineVerdict};
use lmgw_core::config::deployment::{Deployment, StartFacts};
use lmgw_core::machine::probe::NvmlAnswer;
use lmgw_machine::CmdOut;

use crate::support::machine_world::World;

const NVML: &str = "/usr/lib64/libnvidia-ml.so.1";
const RANGED: &str = "         0       1000          1\n         1     524288      65536\n";
/// The image's volumes as the container's mount table lists them.
const MOUNTS: &str = "\
1 0 0:50 / / rw - overlay overlay rw
2 1 0:52 /volumes/lmgw-data/_data /data rw - btrfs /dev/nvme0n1p3 rw
3 1 0:52 /volumes/lmgw-storage/_data /var/lib/containers rw - btrfs /dev/nvme0n1p3 rw
4 1 0:52 /volumes/lmgw-tmp/_data /var/tmp rw - btrfs /dev/nvme0n1p3 rw
";

fn item(
    c: &lmgw_api_types::machine::MachineCheck,
    id: MachineItemId,
) -> &lmgw_api_types::machine::MachineItem {
    c.items.iter().find(|i| i.id == id).unwrap()
}

/// `w`'s gateway is the server image's, started with the card: its control
/// node, and a rootless outer container with a subordinate range.
fn in_container(w: &World) {
    w.state.set_start_for_tests(StartFacts {
        deployment: Deployment::Container,
        ..Default::default()
    });
    w.m.facts
        .file("/dev/nvidiactl", b"")
        .file("/proc/self/uid_map", RANGED)
        .file("/proc/self/gid_map", RANGED)
        .file("/proc/self/mountinfo", MOUNTS);
}

/// The container without the card: no control node, and none of the
/// libraries the host's spec would have mounted.
fn without_card(w: &World) {
    w.m.facts.remove("/dev/nvidiactl");
    w.m.facts.remove(NVML);
}

/// The card passed in, the image's toolkit, Podman inside: ready, with
/// Podman said as it runs on the host, no plan, and each place by volume.
#[tokio::test]
async fn with_the_card_passed_in_the_server_is_ready() {
    let w = World::nvidia().await;
    in_container(&w);
    w.settings(|s| s.router.models_dir = "/data/models".into())
        .await;
    w.all_images_here();
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::Ready, "{c:#?}");
    assert_eq!(c.headline, "This server is ready for local models.");
    assert!(c.plan.is_none());
    assert_eq!(
        item(&c, MachineItemId::GraphicsCard).sentence,
        "Graphics card · NVIDIA GeForce RTX 3090 · 22.5 GiB"
    );
    let running = item(&c, MachineItemId::RunningModels);
    assert_eq!(running.sentence, "lmgw can run models on this server.");
    assert!(
        running.detail[0].starts_with(
            "podman version 5.8.9, inside the lmgw container, its root is host uid 1000, crun, \
             cgroup v2"
        ),
        "{:?}",
        running.detail
    );
    assert!(running
        .detail
        .contains(&"/proc/self/uid_map: 0 1000 1; 1 524288 65536 · /proc/self/gid_map: 0 1000 1; 1 524288 65536".to_string()));
    let room = item(&c, MachineItemId::Room);
    assert_eq!(room.sentence, "Room for models · 466 GiB free.");
    assert!(
        room.detail
            .iter()
            .any(|l| l.starts_with("chat models: /data/models (volume /data) · 466 GiB free")),
        "{:?}",
        room.detail
    );
    assert!(
        room.detail
            .iter()
            .any(|l| l
                .starts_with("builds' scratch and temporary files: /var/tmp (volume /var/tmp) · ")),
        "{:?}",
        room.detail
    );
    // The fake store sits outside the image's volumes, and says so.
    assert!(room
        .detail
        .iter()
        .any(|l| l.contains("on none of the image's volumes")));

    // A host folder mounted over /data/models is not the data volume.
    w.m.facts.file(
        "/proc/self/mountinfo",
        format!("{MOUNTS}5 2 0:53 /srv/models /data/models rw - xfs /dev/sdb1 rw\n"),
    );
    let c = w.check().await;
    let room = item(&c, MachineItemId::Room);
    assert!(
        room.detail.iter().any(|l| l.starts_with(
            "chat models: /data/models (on the mount at /data/models, none of the image's volumes"
        )),
        "{:?}",
        room.detail
    );
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert!(
        toolkit
            .detail
            .iter()
            .any(|l| l.contains("in the server image the toolkit comes with the image")),
        "{:?}",
        toolkit.detail
    );
}

/// The card on the bus, the host's driver running, no control node: the
/// container was started without it, and the host's steps say how.
#[tokio::test]
async fn without_the_card_the_container_says_so() {
    let w = World::nvidia().await;
    in_container(&w);
    without_card(&w);
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    let sentence = "The lmgw container was started without the graphics card. On the host: add \
                    the card to the unit, check that the host's CDI spec is current, restart the \
                    service.";
    assert_eq!(card.sentence, sentence);
    assert_eq!(card.cause, Some(MachineCause::CardNotPassedIn));
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert_eq!(c.headline, sentence);
    assert!(
        card.detail
            .iter()
            .any(|l| l.contains("AddDevice=nvidia.com/gpu=all")
                && l.contains("nvidia-ctk cdi list")
                && l.contains("systemctl --user restart lmgw")),
        "{:?}",
        card.detail
    );
    assert!(card
        .detail
        .contains(&"kernel module 600.20.03 runs on the host".to_string()));
    assert_eq!(
        item(&c, MachineItemId::CardForModels).sentence,
        "Not checked yet: the graphics card comes first."
    );
    assert!(c.plan.is_none());
    assert_eq!(w.probes(), 0, "NVML is not asked without the card");
}

/// The host's driver: changed since the start, not running, passed in
/// without its libraries, or NVML's own mismatch — each the host's to fix,
/// none a restart of this computer.
#[tokio::test]
async fn the_hosts_driver_is_the_hosts_to_fix() {
    // The module newer than the libraries the container started with.
    let w = World::nvidia_at("600.20.03").await;
    in_container(&w);
    w.m.facts.set_driver_libraries("600.10.01");
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(
        card.sentence,
        "On the host: the driver changed (module 600.20.03, libraries 600.10.01). Refresh the \
         host's CDI spec, then restart the lmgw service."
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert!(card
        .detail
        .iter()
        .any(|l| l.contains("nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml")));

    let w = World::nvidia().await;
    in_container(&w);
    w.m.facts.remove("/proc/driver/nvidia/version");
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(
        card.sentence,
        "On the host: NVIDIA's driver is not running. Install or start it there, then restart \
         the lmgw service."
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert!(
        !card.detail.iter().any(|l| l.contains("Secure Boot")),
        "the host's firmware is not this container's to read: {:?}",
        card.detail
    );

    let w = World::nvidia().await;
    in_container(&w);
    w.m.facts.remove(NVML);
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(
        card.sentence,
        "The lmgw container got the graphics card without its driver libraries. On the host: \
         refresh the host's CDI spec, then restart the lmgw service."
    );
    assert_eq!(card.cause, Some(MachineCause::DriverLibrariesMissing));
    assert!(!card.detail.iter().any(|l| l.contains("sudo dnf")));

    // The host's libraries newer than the module it runs: an update the
    // host has not restarted for.
    let w = World::nvidia_at("600.20.03").await;
    in_container(&w);
    w.m.facts.set_driver_libraries("600.30.01");
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    let sentence = "On the host: restart the machine to load the new driver (module 600.20.03, \
                    libraries 600.30.01).";
    assert_eq!(card.sentence, sentence);
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart);
    assert_eq!(c.headline, sentence);
    assert!(
        card.detail
            .iter()
            .any(|l| l.starts_with("On the host: restart the machine")),
        "{:?}",
        card.detail
    );

    // NVML's own mismatch, with no versions to place it: both steps.
    let w = World::nvidia().await;
    in_container(&w);
    *w.nvml.lock().unwrap() = Some(NvmlAnswer::Failed {
        kind: "version_mismatch".into(),
        error: "Driver/library version mismatch".into(),
    });
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(
        card.sentence,
        "On the host: the driver changed since the lmgw container started. If the host has not \
         restarted since its driver update, restart the machine; otherwise refresh the host's \
         CDI spec, then restart the lmgw service."
    );
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert!(card
        .detail
        .iter()
        .any(|l| l.starts_with("On the host: restart the machine")));
    assert!(card
        .detail
        .iter()
        .any(|l| l.starts_with("On the host: refresh the CDI spec")));

    // NVML failing otherwise, and a module version that can't be read: the
    // host's driver to check, never this computer's.
    let w = World::nvidia().await;
    in_container(&w);
    *w.nvml.lock().unwrap() = Some(NvmlAnswer::Failed {
        kind: "unknown".into(),
        error: "Unknown Error".into(),
    });
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(
        card.sentence,
        "lmgw couldn't talk to the graphics card through the host's driver. On the host: check \
         that nvidia-smi answers and that the CDI spec is current, then restart the lmgw service."
    );
    assert!(card.detail.iter().any(|l| l.contains("run nvidia-smi")));

    let w = World::nvidia().await;
    in_container(&w);
    w.m.facts
        .file("/proc/driver/nvidia/version", "no version here\n");
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert!(
        matches!(card.cause, Some(MachineCause::DriverUnreadable { .. })),
        "{card:#?}"
    );
    assert_eq!(
        card.sentence,
        "lmgw couldn't read which graphics driver the host runs. On the host: check that \
         nvidia-smi answers, then restart the lmgw service."
    );
}

/// An AMD card whose render node the container was not given: started
/// without the card, and the toolkit item waits for the card, not a driver.
#[tokio::test]
async fn an_amd_card_not_passed_in_comes_first() {
    let w = World::amd().await;
    in_container(&w);
    w.m.facts.remove("/dev/dri/renderD128").remove("/dev/kfd");
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(card.cause, Some(MachineCause::AmdNoComputeNode));
    assert!(
        card.sentence
            .starts_with("The lmgw container was started without the graphics card."),
        "{}",
        card.sentence
    );
    assert_eq!(
        item(&c, MachineItemId::CardForModels).sentence,
        "Not checked yet: the graphics card comes first."
    );
}

/// The toolkit is the image's: missing is a broken image, a failed write
/// the host's spec first, and there is never a plan or a button.
#[tokio::test]
async fn the_toolkit_is_the_images() {
    let w = World::nvidia().await;
    in_container(&w);
    w.remove_toolkit();
    let c = w.check().await;
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert_eq!(
        toolkit.sentence,
        "One piece is missing so models can use the graphics card: this server image is \
         incomplete. On the host: load a current lmgw server image and restart the service."
    );
    assert_eq!(toolkit.cause, Some(MachineCause::ToolkitMissing));
    assert!(c.plan.is_none(), "{:?}", c.plan);
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert!(toolkit
        .detail
        .iter()
        .any(|l| l.contains("podman load -i lmgw-server-<version>.oci.tar")));

    let w = World::nvidia().await;
    in_container(&w);
    w.m.fail_with(Some(CmdOut::failure(1, "failed to initialize NVML\n")));
    let prefix = w.state.snapshot().settings.container_prefix.clone();
    let _ = w
        .state
        .cdi()
        .ensure_fresh(&prefix, std::time::Duration::from_secs(60))
        .await;
    let c = w.check().await;
    let toolkit = item(&c, MachineItemId::CardForModels);
    assert!(
        toolkit.sentence.starts_with(
            "lmgw couldn't write the description of your graphics card that models \
                          use. On the host: check that the host's CDI spec is current"
        ),
        "{}",
        toolkit.sentence
    );
    assert!(c.plan.is_none());
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
}

/// Podman inside the lmgw container: rootful on the host or its root a host
/// uid, a map without a range named as the service account's, no subuid
/// hints for the inner root, a missing Podman a broken image.
#[tokio::test]
async fn podman_inside_is_said_as_it_runs_on_the_host() {
    let w = World::nvidia().await;
    in_container(&w);
    w.all_images_here();
    w.m.facts.file("/proc/self/uid_map", "0 0 4294967295\n");
    w.m.facts.file("/proc/self/gid_map", "0 0 4294967295\n");
    let c = w.check().await;
    assert!(
        item(&c, MachineItemId::RunningModels).detail[0].contains("rootful on the host"),
        "{:?}",
        item(&c, MachineItemId::RunningModels).detail
    );
    assert_eq!(c.verdict, MachineVerdict::Ready);

    // A rootful Podman with a user namespace of its own maps root to a high
    // uid: that uid is named, not a rootless account.
    w.m.facts
        .file("/proc/self/uid_map", "0 100000 65536\n")
        .file("/proc/self/gid_map", "0 100000 65536\n");
    let c = w.check().await;
    let first = &item(&c, MachineItemId::RunningModels).detail[0];
    assert!(first.contains("its root is host uid 100000"), "{first}");
    assert!(!first.contains("rootless"), "{first}");

    w.m.facts.file("/proc/self/uid_map", "0 1000 1\n");
    let c = w.check().await;
    let running = item(&c, MachineItemId::RunningModels);
    let no_range = "lmgw can't start models on this server yet. On the host: the service account \
                    needs subuid and subgid ranges.";
    assert_eq!(running.sentence, no_range);
    assert_eq!(c.headline, no_range);
    assert_eq!(c.verdict, MachineVerdict::NeedsSteps);
    assert!(running
        .detail
        .iter()
        .any(|l| l.contains("usermod --add-subuids") && l.contains("podman system migrate")));

    // A failing Podman: its error, and none of the desktop's hints about
    // an account inside the image.
    w.m.facts.file("/proc/self/uid_map", RANGED);
    *w.podman.info_fails.lock().unwrap() =
        Some("Error: cannot set up namespace using newuidmap\n".into());
    w.m.facts.set_user_name("someone");
    let c = w.check().await;
    let running = item(&c, MachineItemId::RunningModels);
    assert_eq!(
        running.sentence,
        "lmgw can't start models on this server yet."
    );
    assert!(
        !running.detail.iter().any(|l| l.contains("/etc/subuid")),
        "{:?}",
        running.detail
    );
    assert!(running.detail[0].starts_with("Podman inside the lmgw container: "));

    *w.podman.info_fails.lock().unwrap() = None;
    *w.podman.missing.lock().unwrap() = true;
    let c = w.check().await;
    let running = item(&c, MachineItemId::RunningModels);
    assert_eq!(running.cause, Some(MachineCause::PodmanMissing));
    assert!(running
        .detail
        .iter()
        .any(|l| l.contains("load a current lmgw server image")));
    assert!(!running.detail.iter().any(|l| l.contains("dnf install")));
}

/// The same states on the desktop keep the desktop's words: a restart of
/// the computer, the driver's missing part with its package, the plan for
/// the toolkit, Podman on this computer, the rooms without volumes.
#[tokio::test]
async fn the_desktop_keeps_its_words() {
    let w = World::nvidia().await;
    w.all_images_here();
    assert_eq!(
        w.check().await.headline,
        "This computer is ready for local models."
    );

    let w = World::nvidia_at("600.10.01").await;
    w.m.facts.set_driver_libraries("600.20.03");
    let c = w.check().await;
    assert_eq!(c.verdict, MachineVerdict::NeedsRestart);
    assert_eq!(
        item(&c, MachineItemId::GraphicsCard).sentence,
        "Restart the computer to finish the graphics driver update."
    );

    let w = World::nvidia().await;
    without_card(&w);
    let c = w.check().await;
    let card = item(&c, MachineItemId::GraphicsCard);
    assert_eq!(
        card.sentence,
        "A part of the graphics driver that local models need is missing. Here's how to add it."
    );
    assert!(card.detail.iter().any(|l| l.contains("sudo dnf install")));

    let w = World::nvidia().await;
    w.m.facts.remove("/proc/driver/nvidia/version");
    let c = w.check().await;
    assert!(
        item(&c, MachineItemId::GraphicsCard)
            .sentence
            .starts_with("The graphics driver is "),
        "{}",
        item(&c, MachineItemId::GraphicsCard).sentence
    );

    let w = World::nvidia().await;
    w.remove_toolkit();
    let c = w.check().await;
    assert_eq!(
        item(&c, MachineItemId::CardForModels).sentence,
        "One piece is missing so models can use the graphics card. Here's how to add it."
    );
    assert!(c.plan.is_some(), "the desktop keeps its plan");

    let w = World::nvidia().await;
    w.settings(|s| s.router.models_dir = "/data/models".into())
        .await;
    w.m.facts
        .file("/proc/self/uid_map", "0 1000 1\n")
        .file("/proc/self/gid_map", "0 1000 1\n");
    let c = w.check().await;
    let running = item(&c, MachineItemId::RunningModels);
    assert_eq!(running.sentence, "lmgw can run models on this computer.");
    assert!(running.detail[0].starts_with("podman version 5.8.9, rootless, crun"));
    assert!(!running.detail.iter().any(|l| l.contains("uid_map")));
    let room = item(&c, MachineItemId::Room);
    assert!(!room.detail.iter().any(|l| l.contains("(volume")));
    assert!(!room.detail.iter().any(|l| l.contains("/var/tmp")));
}

/// In the server image the metered probe is not run (headless server
/// design §7): nmcli is never asked, whether the connection is metered is
/// not known, and the pre-fetch goes ahead where the desktop, reading the
/// same connection as metered, skips it.
#[tokio::test]
async fn in_the_server_image_the_metered_probe_is_not_run() {
    use lmgw_core::machine::{Metered, Prefetched};
    let metered = |w: &World| {
        w.m.facts.file(
            "/proc/net/route",
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
             wwan0\t00000000\t010200C0\t0003\t0\t0\t700\t00000000\t0\t0\t0\n",
        );
        w.m.facts.program("/usr/bin/nmcli", |_| {
            Ok(CmdOut::success("GENERAL.METERED:yes\n"))
        });
    };

    let w = World::nvidia().await;
    in_container(&w);
    metered(&w);
    w.all_images_here();
    assert_eq!(w.check().await.verdict, MachineVerdict::Ready);
    let done = w.state.machine.prefetched().await.expect("it began");
    assert!(
        !done.iter().any(|p| matches!(p, Prefetched::Metered { .. })),
        "{done:?}"
    );
    assert_eq!(
        lmgw_core::machine::connection_metered(&w.state).await,
        Metered::Unknown(lmgw_core::machine::METERED_NOT_CHECKED.into())
    );
    assert!(w.m.facts.calls_of("/usr/bin/nmcli").is_empty());

    // The desktop reads the same connection as metered.
    let w = World::nvidia().await;
    metered(&w);
    w.all_images_here();
    assert_eq!(w.check().await.verdict, MachineVerdict::Ready);
    let done = w.state.machine.prefetched().await.expect("it began");
    assert!(
        matches!(&done[..], [Prefetched::Metered { .. }]),
        "{done:?}"
    );
}
