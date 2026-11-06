//! A stated computer for the machine check (machine-check design §6,
//! "Driver facts", "Core"): `cdi_machine`'s NVIDIA machine (driver, spec
//! paths, `nvidia-ctk`, a temp `XDG_RUNTIME_DIR`) plus what the check reads
//! beyond the spec — the PCI bus, rpm and its keys, os-release, dnf5, the
//! NVML probe started as `/proc/self/exe`, a file system — a podman that
//! answers `--version`, `--help`, `info`, `image exists`, `images` (with
//! the build labels a test gives), the throwaway listing of an image's ggml
//! libraries, and `stop`/`wait`/`rm` for a container a test put in the
//! registry, and a registry (a mock serving ghcr.io) for the download's
//! size.
//!
//! Nothing of the computer the suite runs on is read or run.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::erased::Erased;
use lmgw_core::machine::probe::{NvmlAnswer, NvmlCard};
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner, Registry};
use lmgw_core::state::{AppState, SharedState};
use lmgw_machine::CmdOut;

use super::cdi_machine::CdiMachine;

/// The RTX 3090's PCI address and IDs on the stated bus.
pub const NVIDIA_PCI: &str = "0000:01:00.0";

/// Podman's image store on the stated machine.
pub const GRAPH_ROOT: &str = "/home/someone/.local/share/containers/storage";

/// `rpm -qf`'s line for NVIDIA's `-base` 1.20.1, signed by NVIDIA.
pub const NVIDIA_OWNER: &str = "nvidia-container-toolkit-base\t(none)\t1.20.1\t1\tx86_64\tRSA/SHA512, Fri Sep 18 23:09:15 2026, Key ID ddcae044f796ecb0\t(none)\n";

/// The podman the check asks, with an image store a test sets.
#[derive(Default)]
pub struct CheckPodman {
    /// Not installed: every call is "not found".
    pub missing: Mutex<bool>,
    /// `podman info` fails with this.
    pub info_fails: Mutex<Option<String>>,
    /// `--help` lists no `--cdi-spec-dir`.
    pub old: Mutex<bool>,
    /// The images on the machine.
    pub images: Mutex<BTreeSet<String>>,
    /// An image's labels (`dev.lmgw.backend`), by its name.
    pub labels: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    /// The `libggml*` files an image's listing finds, one per line, by its
    /// name; none for an image not named here.
    pub libraries: Mutex<BTreeMap<String, String>>,
    pub calls: Mutex<Vec<Vec<String>>>,
}

/// The ID the stated store gives image `name`: 64 hex digits of its own.
pub fn image_id(name: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    format!("{:016x}", h.finish()).repeat(4)
}

impl CommandRunner for CheckPodman {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        Box::pin(async move {
            assert_eq!(program, "podman");
            self.calls.lock().unwrap().push(args.to_vec());
            if *self.missing.lock().unwrap() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No such file or directory (os error 2)",
                ));
            }
            let ok = |stdout: &str| CmdOutput {
                status: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            };
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            Ok(match a.as_slice() {
                ["--version"] => ok("podman version 5.8.9\n"),
                ["--help"] => {
                    let mut text =
                        String::from("Usage:\n  podman [options] [command]\n\nOptions:\n");
                    if !*self.old.lock().unwrap() {
                        text.push_str(
                            "      --cdi-spec-dir stringArray    Set the CDI spec directory path\n",
                        );
                    }
                    ok(&text)
                }
                ["info", "--format", "json"] => match self.info_fails.lock().unwrap().clone() {
                    Some(e) => CmdOutput {
                        status: 125,
                        stdout: String::new(),
                        stderr: e,
                    },
                    None => ok(&format!(
                        r#"{{"host":{{"security":{{"rootless":true}},"ociRuntime":{{"name":"crun"}},"cgroupVersion":"v2"}},"store":{{"graphRoot":"{GRAPH_ROOT}","graphDriverName":"overlay"}}}}"#,
                    )),
                },
                ["image", "exists", "--", reference] => CmdOutput {
                    status: if self.images.lock().unwrap().contains(*reference) {
                        0
                    } else {
                        1
                    },
                    stdout: String::new(),
                    stderr: String::new(),
                },
                ["images", "--format", "json"] => {
                    let labels = self.labels.lock().unwrap();
                    let rows: Vec<serde_json::Value> = self
                        .images
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|name| {
                            serde_json::json!({
                                "Id": image_id(name),
                                "Names": [name],
                                "Labels": labels.get(name).cloned().unwrap_or_default(),
                            })
                        })
                        .collect();
                    ok(&serde_json::Value::from(rows).to_string())
                }
                // An image's ggml libraries, listed by a throwaway container
                // of it (`backends::images::image_gpu`).
                [.., "--entrypoint", "find", id, "/", "-xdev", "-name", "libggml*"] => {
                    let images = self.images.lock().unwrap();
                    let name = images.iter().find(|n| image_id(n) == *id);
                    let listing = name
                        .and_then(|n| self.libraries.lock().unwrap().get(n).cloned())
                        .unwrap_or_default();
                    ok(&listing)
                }
                // A container a test put in the registry, stopped.
                ["stop", ..] | ["wait", ..] | ["rm", ..] => ok(""),
                other => panic!("the check asks podman nothing else: {other:?}"),
            })
        })
    }
}

/// A gateway whose check reads a stated machine.
pub struct World {
    pub state: SharedState,
    pub m: CdiMachine,
    pub podman: Arc<CheckPodman>,
    /// The NVML probe's answer; `None`: the probe is not there.
    pub nvml: Arc<Mutex<Option<NvmlAnswer>>>,
    pub probes: Arc<AtomicUsize>,
    /// How long the probe takes.
    pub probe_delay: Arc<Mutex<Duration>>,
    /// Told each time a probe starts, when a test listens.
    pub probe_started: Arc<Mutex<Option<std::sync::mpsc::Sender<()>>>>,
    /// `rpm -qf /usr/bin/nvidia-ctk`'s line; `None`: not owned.
    pub owner: Arc<Mutex<Option<String>>>,
    pub data_dir: tempfile::TempDir,
    /// The registry the engine images come from: ghcr.io, answering 404 for
    /// every manifest until a test serves one ([`Self::manifest`]).
    pub registry: wiremock::MockServer,
}

/// An RTX 3090 as NVML reports one: a test card whose driver-reported total
/// is below the nominal 24 GiB (its UUID made up).
pub fn rtx_3090() -> NvmlAnswer {
    NvmlAnswer::Ok {
        driver_version: Some("600.20.03".into()),
        cuda_version: Some(13020),
        cards: vec![NvmlCard {
            index: 0,
            name: "NVIDIA GeForce RTX 3090".into(),
            uuid: Some("GPU-00000000-0000-4000-8000-000000000001".into()),
            memory_total: Some(24_146_608_128),
            pci_bus_id: Some("00000000:01:00.0".into()),
        }],
    }
}

/// The stated machine alone, without a gateway: what a world built for
/// something else (the admission fixtures of `vram_admission`) states on its
/// own gateway's check ([`Self::state_on`]).
pub struct Machine {
    pub m: CdiMachine,
    pub nvml: Arc<Mutex<Option<NvmlAnswer>>>,
    pub probes: Arc<AtomicUsize>,
    pub probe_delay: Arc<Mutex<Duration>>,
    pub probe_started: Arc<Mutex<Option<std::sync::mpsc::Sender<()>>>>,
    pub owner: Arc<Mutex<Option<String>>>,
}

impl Machine {
    /// [`World::nvidia`]'s machine.
    pub fn nvidia() -> Self {
        Self::nvidia_at("600.20.03")
    }

    pub fn nvidia_at(version: &str) -> Self {
        let m = CdiMachine::at(version);
        let f = &m.facts;
        f.file(
            "/etc/os-release",
            "NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=44\n",
        );
        f.file_mode("/usr/bin/dnf5", b"\x7fELF", 0o755);
        f.link("/usr/bin/dnf", "dnf5");
        f.file(
            "/usr/share/hwdata/pci.ids",
            "10de  NVIDIA Corporation\n\t2204  GA102 [GeForce RTX 3090]\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t15bf  Phoenix1\n8086  Intel Corporation\n\ta780  Raptor Lake-S GT1 [UHD Graphics 770]\n",
        );
        World::pci(f, NVIDIA_PCI, 0x10de, 0x2204, Some("nvidia"));
        f.set_space("/", 500_000_000_000, 2_000_000_000_000, 1);
        let owner = Arc::new(Mutex::new(Some(NVIDIA_OWNER.to_string())));
        let o = owner.clone();
        f.program("/usr/bin/rpm", move |args| {
            let target = args.last().map(String::as_str).unwrap_or("");
            Ok(match (args[0].as_str(), target) {
                ("-qf", "/usr/bin/nvidia-ctk") => match o.lock().unwrap().clone() {
                    Some(line) => CmdOut::success(line),
                    None => {
                        CmdOut::failure(1, "file /usr/bin/nvidia-ctk is not owned by any package\n")
                    }
                },
                ("-q", "xorg-x11-drv-nvidia") => CmdOut::success(
                    "xorg-x11-drv-nvidia\t3\t600.20.03\t1.fc44\tx86_64\t(none)\t(none)\n",
                ),
                ("-q", name) => CmdOut::failure(1, format!("package {name} is not installed\n")),
                _ => CmdOut::failure(1, "error: unexpected query\n"),
            })
        });
        f.reply(
            "/usr/bin/rpmkeys",
            CmdOut::success(
                "c95b321b61e88c1809c4f759ddcae044f796ecb0 NVIDIA CORPORATION (Open Source Projects) <cudatools@nvidia.com> public key\n",
            ),
        );
        let nvml = Arc::new(Mutex::new(Some(rtx_3090())));
        let probes = Arc::new(AtomicUsize::new(0));
        let probe_delay = Arc::new(Mutex::new(Duration::ZERO));
        let probe_started: Arc<Mutex<Option<std::sync::mpsc::Sender<()>>>> = Arc::default();
        let (n, p, d, started) = (
            nvml.clone(),
            probes.clone(),
            probe_delay.clone(),
            probe_started.clone(),
        );
        f.program("/proc/self/exe", move |args| {
            assert_eq!(args, ["--machine-probe", "nvml"]);
            p.fetch_add(1, Ordering::SeqCst);
            if let Some(tx) = started.lock().unwrap().as_ref() {
                let _ = tx.send(());
            }
            std::thread::sleep(*d.lock().unwrap());
            Ok(match n.lock().unwrap().clone() {
                Some(answer @ NvmlAnswer::Ok { .. }) => {
                    CmdOut::success(serde_json::to_string(&answer).unwrap() + "\n")
                }
                Some(answer) => CmdOut {
                    status: Some(1),
                    stdout: serde_json::to_string(&answer).unwrap() + "\n",
                    stderr: String::new(),
                },
                None => CmdOut::failure(127, "no such program"),
            })
        });

        Self {
            m,
            nvml,
            probes,
            probe_delay,
            probe_started,
            owner,
        }
    }

    /// This machine is what `state`'s check reads (amdgpu: none).
    pub fn state_on(&self, state: &SharedState) {
        state
            .machine
            .set_machine(self.m.facts.clone(), Arc::new(|| None));
    }

    pub fn probes(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }
}

/// What a podman that works answers the check (§1.3): `--version`,
/// `--help` with `--cdi-spec-dir`, and `info --format json`, as
/// [`CheckPodman`] answers by default; `None` for anything else. For the
/// fake podman of another world, so its gateway's check comes out ready.
pub fn working_podman(args: &[String]) -> Option<CmdOutput> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let ok = |stdout: String| CmdOutput {
        status: 0,
        stdout,
        stderr: String::new(),
    };
    match a.as_slice() {
        ["--version"] => Some(ok("podman version 5.8.9\n".into())),
        ["--help"] => Some(ok(
            "Usage:\n  podman [options] [command]\n\nOptions:\n      --cdi-spec-dir \
             stringArray    Set the CDI spec directory path\n"
                .into(),
        )),
        ["info", "--format", "json"] => Some(ok(format!(
            r#"{{"host":{{"security":{{"rootless":true}},"ociRuntime":{{"name":"crun"}},"cgroupVersion":"v2"}},"store":{{"graphRoot":"{GRAPH_ROOT}","graphDriverName":"overlay"}}}}"#,
        ))),
        _ => None,
    }
}

impl World {
    /// Fedora 44 with a working RTX 3090 at driver 600.20.03 and NVIDIA's
    /// toolkit: a ready machine.
    pub async fn nvidia() -> Self {
        Self::nvidia_at("600.20.03").await
    }

    pub async fn nvidia_at(version: &str) -> Self {
        let Machine {
            m,
            nvml,
            probes,
            probe_delay,
            probe_started,
            owner,
        } = Machine::nvidia_at(version);
        let state = AppState::init_for_tests().await.unwrap();
        let data_dir = tempfile::Builder::new()
            .prefix("lmgw-machine-data-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        state.set_cdi_for_tests(m.own_spec(data_dir.path()));
        state
            .machine
            .set_machine(m.facts.clone(), Arc::new(|| None));
        let podman = Arc::new(CheckPodman::default());
        state.set_runtime_for_tests(Arc::new(Registry::new(
            podman.clone(),
            reqwest::Client::new(),
        )));
        let registry = wiremock::MockServer::start().await;
        state.builds.set_registry_client(
            lmgw_core::backends::oci::RegistryClient::new()
                .unwrap()
                .with_endpoint("ghcr.io", registry.uri()),
        );
        Self {
            state,
            m,
            podman,
            nvml,
            probes,
            probe_delay,
            probe_started,
            owner,
            data_dir,
            registry,
        }
    }

    /// ghcr.io serves `repository:tag`'s amd64 image manifest: the config
    /// blob `config` (7000 bytes) and `layers`, each `(digest, compressed
    /// size)`.
    pub async fn manifest(
        &self,
        repository: &str,
        tag: &str,
        config: &str,
        layers: &[(&str, u64)],
    ) {
        use wiremock::matchers::{method, path};
        let layers: Vec<serde_json::Value> = layers
            .iter()
            .map(|(digest, size)| {
                serde_json::json!({
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": digest,
                    "size": size,
                })
            })
            .collect();
        let body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config,
                "size": 7000,
            },
            "layers": layers,
        });
        wiremock::Mock::given(method("GET"))
            .and(path(format!("/v2/{repository}/manifests/{tag}")))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(
                body.to_string(),
                "application/vnd.oci.image.manifest.v1+json",
            ))
            .mount(&self.registry)
            .await;
    }

    /// Change the settings as `edit` says, and reload them.
    pub async fn settings(&self, edit: impl FnOnce(&mut lmgw_core::config::Settings)) {
        let mut s = self.state.snapshot().settings.clone();
        edit(&mut s);
        lmgw_core::store::save_settings(&self.state.db, &s)
            .await
            .unwrap();
        self.state.reload_snapshot().await.unwrap();
    }

    /// Every engine image the default settings name is on the machine.
    pub fn all_images_here(&self) {
        let s = self.state.snapshot().settings.clone();
        let mut images = self.podman.images.lock().unwrap();
        for i in [
            s.router.image,
            s.aux_router.image.clone(),
            s.audio.image,
            s.image.image,
        ] {
            images.insert(i);
        }
    }

    /// A display device on the stated PCI bus, bound to `driver`.
    pub fn pci(
        f: &lmgw_machine::fake::FakeFacts,
        address: &str,
        vendor: u16,
        device: u16,
        driver: Option<&str>,
    ) {
        let dir = format!("/sys/bus/pci/devices/{address}");
        f.file(format!("{dir}/class"), "0x030000\n")
            .file(format!("{dir}/vendor"), format!("0x{vendor:04x}\n"))
            .file(format!("{dir}/device"), format!("0x{device:04x}\n"));
        if let Some(d) = driver {
            f.dir(format!("/sys/bus/pci/drivers/{d}"));
            f.link(
                format!("{dir}/driver"),
                format!("../../../bus/pci/drivers/{d}"),
            );
        }
    }

    /// The machine check, fresh.
    pub async fn check(&self) -> lmgw_api_types::machine::MachineCheck {
        (*lmgw_core::machine::check(&self.state, true).await).clone()
    }

    /// [`Self::nvidia`]'s computer with its card swapped for an AMD Radeon
    /// 780M (Phoenix1) on amdgpu, with its compute node, a render node and
    /// amdgpu's counters (16 GB): the card is ready for models.
    pub async fn amd() -> Self {
        let w = Self::nvidia().await;
        w.amd_card();
        w
    }

    /// The card on the stated bus is the AMD one of [`Self::amd`].
    pub fn amd_card(&self) {
        let f = &self.m.facts;
        f.remove("/sys/bus/pci/devices");
        Self::pci(f, "0000:c5:00.0", 0x1002, 0x15bf, Some("amdgpu"));
        f.file("/dev/kfd", b"").file("/dev/dri/renderD128", b"");
        self.state.machine.set_machine(
            self.m.facts.clone(),
            Arc::new(|| {
                Some(Ok(vec![lmgw_core::vram::GpuMemory {
                    index: 0,
                    name: "amdgpu card1".into(),
                    total_bytes: 16_000_000_000,
                    used_bytes: 1024 * 1024 * 1024,
                    free_bytes: 15 * 1024 * 1024 * 1024,
                }]))
            }),
        );
    }

    /// [`Self::amd_card`]'s card without its compute node `/dev/kfd`: the
    /// render nodes alone, which a Vulkan engine computes through.
    pub fn amd_card_without_kfd(&self) {
        self.amd_card();
        self.m.facts.remove("/dev/kfd");
    }

    /// [`Self::amd_card`]'s card beside the NVIDIA one (which §1.1 judges
    /// first), with its compute node and a render node.
    pub fn add_amd_card(&self) {
        let f = &self.m.facts;
        Self::pci(f, "0000:c5:00.0", 0x1002, 0x15bf, Some("amdgpu"));
        f.file("/dev/kfd", b"").file("/dev/dri/renderD128", b"");
    }

    /// The card on the stated bus is an Intel UHD 770 on i915, with its
    /// render node: a vendor lmgw has no settings for.
    pub fn intel_card(&self) {
        let f = &self.m.facts;
        f.remove("/sys/bus/pci/devices");
        Self::pci(f, "0000:00:02.0", 0x8086, 0xa780, Some("i915"));
        f.file("/dev/dri/renderD128", b"");
    }

    /// No display device on the stated bus.
    pub fn no_card(&self) {
        self.m.facts.remove("/sys/bus/pci/devices");
    }

    /// NVIDIA's toolkit gone: no `nvidia-ctk`, no owner.
    pub fn remove_toolkit(&self) {
        self.m.facts.uninstall(lmgw_machine::cdi::own::NVIDIA_CTK);
        *self.owner.lock().unwrap() = None;
    }

    pub fn probes(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }
}

/// Every plain sentence of `c`: the headline and each item's.
pub fn sentences(c: &lmgw_api_types::machine::MachineCheck) -> Vec<String> {
    std::iter::once(c.headline.clone())
        .chain(c.items.iter().map(|i| i.sentence.clone()))
        .collect()
}
