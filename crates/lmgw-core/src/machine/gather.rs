//! Reading the machine for the check (machine-check design §1): the facts
//! through lmgw-machine's [`Facts`] seam on a blocking thread, Podman through
//! the registry's runner, lmgw's own spec through [`OwnSpec`]. Nothing here
//! writes anything; the judgements are in the sibling modules.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lmgw_machine::cdi::own::{judge, Judgement, OnDisk, Stamp, NVIDIA_CTK};
use lmgw_machine::driver::{DriverState, ModuleFile, SecureBoot};
use lmgw_machine::pci::{display_devices, PciDevice, Vendor};
use lmgw_machine::plan::{Inputs, PROVIDERS};
use lmgw_machine::repos::{cache_dir_id, cache_times, CacheTime};
use lmgw_machine::rpm::{query, Package};
use lmgw_machine::{Facts, Space};

use super::probe::{self, NvmlAnswer};
use crate::config::deployment::Deployment;

mod images;
use crate::runtime::cdi::{DefaultDirs, LastRun, SpecPaths};
use crate::runtime::Class;
use crate::state::AppState;
use crate::vram::GpuMemory;
#[cfg(test)]
pub(super) use images::Image;
pub(super) use images::Images;

/// The driver packages whose presence tells "installed, not running" from
/// "not installed" (§1.1).
const DRIVER_PACKAGES: [&str; 4] = [
    "xorg-x11-drv-nvidia",
    "akmod-nvidia",
    "kmod-nvidia",
    "xorg-x11-drv-nvidia-cuda-libs",
];

/// amdgpu's compute node, and the dir of the render nodes.
pub(super) const KFD: &str = "/dev/kfd";
pub(super) const DRI: &str = "/dev/dri";

/// §1.1's facts.
pub(super) struct CardFacts {
    pub pci: Vec<PciDevice>,
    pub driver: DriverState,
    pub secure_boot: SecureBoot,
    pub driver_packages: Vec<Package>,
    /// The NVIDIA module built for the running kernel and when this boot
    /// started (`/proc/stat`'s `btime`), read when no module runs: whether
    /// Secure Boot is the one keeping it from loading (§1.1).
    pub module: Option<ModuleFile>,
    pub boot_ns: Option<i128>,
    /// The NVML probe's answer; `None` when it was not asked (no NVIDIA
    /// card, or the driver's halves do not agree).
    pub nvml: Option<Result<NvmlAnswer, String>>,
    /// amdgpu's counters; `None` without an AMD card.
    pub amd: Option<Result<Vec<GpuMemory>, String>>,
    pub kfd: bool,
    pub render_nodes: Vec<String>,
    /// Whether `/dev/nvidiactl` is there: in the server image, whether the
    /// container was started with the NVIDIA card (headless server design
    /// §7).
    pub nvidia_nodes: bool,
}

impl CardFacts {
    pub fn has(&self, vendor: Vendor) -> bool {
        self.pci.iter().any(|d| d.vendor() == vendor)
    }
}

/// lmgw's own spec as it is now (§1.2), read without writing.
pub(super) struct SpecFacts {
    pub default_dirs: Option<DefaultDirs>,
    pub last: Option<LastRun>,
    pub paths: Option<SpecPaths>,
    /// The judgement now, from the files on disk; `None` when lmgw manages
    /// no spec.
    pub now: Option<Judgement>,
    pub stamp: Option<Stamp>,
}

/// NVIDIA's software source not loading (§3.2, "A section that stopped
/// loading").
pub(super) struct Frozen {
    pub section: String,
    pub last_loaded_ns: i128,
    pub others_loaded_ns: i128,
    pub expire_secs: u64,
}

/// The advisory `dnf repoquery -C` (§3.2): information, never a plan input.
#[derive(Default)]
pub(super) struct RepoQuery {
    /// `(name, evr, repo, download bytes)` rows.
    pub rows: Vec<(String, String, String, Option<u64>)>,
    /// Why it said nothing, when it did not run or failed.
    pub skipped: Option<String>,
}

/// §1.2's facts, read only for an NVIDIA card.
pub(super) struct ToolkitFacts {
    pub inputs: Inputs,
    pub helper: Result<(), String>,
    pub appimage: bool,
    /// `nvidia-ctk --version`'s version, or why it gave none.
    pub ctk_version: Result<String, String>,
    pub spec: SpecFacts,
    /// The system's configured spec dirs, and the file that defines
    /// `nvidia.com/gpu=all` in them.
    pub system_dirs: Vec<PathBuf>,
    pub system_all: Option<PathBuf>,
    pub frozen: Option<Frozen>,
    pub repoquery: RepoQuery,
    /// The plans the root helper read differently (D88).
    pub read_differently: Vec<String>,
}

/// What `podman info` says (§1.3).
pub(super) struct PodmanInfo {
    pub rootless: Option<bool>,
    pub oci_runtime: Option<String>,
    pub cgroup_version: Option<String>,
    pub graph_root: Option<String>,
    /// The storage driver (`overlay`), whose layer list is
    /// `<graph_root>/<driver>-layers/layers.json`.
    pub graph_driver: Option<String>,
}

/// §1.3's facts.
pub(super) struct PodmanFacts {
    /// `podman --version`'s line; `Err(None)` when podman is not there.
    pub version: Result<String, Option<String>>,
    /// Whether `podman --help` lists `--cdi-spec-dir`.
    pub has_option: Option<bool>,
    pub info: Result<PodmanInfo, String>,
    /// The user's subordinate ID ranges, read when `podman info` failed:
    /// `(in /etc/subuid, in /etc/subgid)`; never in the server image, whose
    /// Podman runs as the container's root.
    pub subids: Option<(bool, bool)>,
    /// In the server image: the container's user and group ID maps, which
    /// say how it runs on the host (headless server design §7).
    pub id_maps: Option<super::container::IdMaps>,
}

/// A class's or a row's run arguments and image (§1.4, §1.6).
pub(super) struct RunArgs {
    pub class: Class,
    /// `None` for the class's defaults, the model id for a row.
    pub model: Option<String>,
    pub args: Vec<String>,
    pub image: String,
}

/// A place lmgw keeps files in, and its file system (§1.5).
pub(super) struct Room {
    pub what: &'static str,
    pub path: String,
    pub space: Result<Space, String>,
}

/// Everything the check read.
pub(super) struct Gathered {
    pub card: CardFacts,
    pub toolkit: Option<ToolkitFacts>,
    pub podman: PodmanFacts,
    pub rooms: Vec<Room>,
    pub images: Images,
    /// The settings and rows §1.4 and its switch are judged from, and the
    /// images and rooms above were read for (WP12).
    pub snap: Arc<crate::config::Snapshot>,
    /// What §1.4 reads beyond the settings: the AMD card, what the images
    /// are built for, the running models.
    pub present: super::fit::present::Present,
    /// Where this lmgw runs: the server image's container words its items
    /// for the host (headless server design §7).
    pub deployment: Deployment,
    /// In the server image: the container's mount points, which say the
    /// volume each room lies on (§7).
    pub mounts: Option<super::container::Mounts>,
}

/// A program the check ran within the load timeout, failed: the timeout
/// named with its setting, anything else as it is. `what` is the program
/// as a person knows it ("the graphics card check").
pub(super) fn limited(what: &str, e: &std::io::Error, limit: Duration) -> String {
    if e.kind() == std::io::ErrorKind::TimedOut {
        format!(
            "{what} did not finish within {} s, the load timeout ({}), and was stopped",
            limit.as_secs(),
            crate::config::LOAD_TIMEOUT_LABEL
        )
    } else {
        format!("{what}: {e}")
    }
}

/// The render nodes in `/dev/dri` (`renderD128`).
pub(super) fn render_nodes(f: &dyn Facts) -> Vec<String> {
    f.list(Path::new(DRI))
        .map(|n| n.into_iter().filter(|n| n.starts_with("renderD")).collect())
        .unwrap_or_default()
}

/// The reads that need only the facts: §1.1 without NVML and amdgpu.
fn card_facts(f: &dyn Facts) -> CardFacts {
    let pci = display_devices(f);
    let driver_packages = DRIVER_PACKAGES
        .iter()
        .filter_map(|name| query(f, name).ok())
        .flatten()
        .collect();
    let render_nodes = render_nodes(f);
    let driver = DriverState::read(f);
    let no_module = matches!(driver, DriverState::NoModule { .. });
    CardFacts {
        module: no_module.then(|| lmgw_machine::driver::module_file(f)),
        boot_ns: no_module
            .then(|| lmgw_machine::driver::boot_time_ns(f))
            .flatten(),
        driver,
        secure_boot: lmgw_machine::driver::secure_boot(f),
        driver_packages,
        nvml: None,
        amd: None,
        kfd: lmgw_machine::facts::exists(f, Path::new(KFD)),
        nvidia_nodes: lmgw_machine::facts::exists(f, Path::new(super::container::NVIDIACTL)),
        render_nodes,
        pci,
    }
}

/// When dnf last loaded NVIDIA's section against the system's own: a
/// section whose metadata expired by its own `metadata_expire` while
/// `fedora` or `updates` were loaded after that has stopped loading.
/// Compared by [`CacheTime::checked_ns`], the primary metadata file's time
/// that dnf5 itself ages its cache by, never by `repomd.xml`'s time, which
/// is when a source last published (`fedora`'s is its release day).
fn frozen(inputs: &Inputs, times: &[CacheTime]) -> Option<Frozen> {
    let section = inputs
        .repos
        .sections
        .iter()
        .filter(|s| s.enabled)
        .find(|s| {
            s.baseurl
                .iter()
                .any(|u| u.starts_with(lmgw_machine::plan::embedded::NVIDIA_STABLE_PREFIX))
        })?;
    let mine = times.iter().find(|t| t.id == section.id)?;
    let others = times
        .iter()
        .filter(|t| t.id == "fedora" || t.id == "updates")
        .map(|t| t.checked_ns)
        .max()?;
    let expire_secs = expire_secs(section.metadata_expire.as_deref());
    let expired_at = mine.checked_ns + i128::from(expire_secs) * 1_000_000_000;
    (others > expired_at).then(|| Frozen {
        section: section.id.clone(),
        last_loaded_ns: mine.checked_ns,
        others_loaded_ns: others,
        expire_secs,
    })
}

/// dnf's `metadata_expire`: seconds, or with an `s`, `m`, `h` or `d`
/// suffix; `-1` and `never` never expire. dnf5's default is 48 h.
pub(super) fn expire_secs(value: Option<&str>) -> u64 {
    const DEFAULT: u64 = 48 * 3600;
    let Some(v) = value.map(str::trim) else {
        return DEFAULT;
    };
    if v == "-1" || v.eq_ignore_ascii_case("never") {
        return u64::MAX / 2_000_000_000;
    }
    let (num, unit) = match v.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (&v[..i], c.to_ascii_lowercase()),
        _ => (v, 's'),
    };
    let Ok(n) = num.trim().parse::<u64>() else {
        return DEFAULT;
    };
    n.saturating_mul(match unit {
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => 1,
    })
}

/// The advisory `dnf repoquery -C`, as the user, from the user's own
/// metadata cache: the toolkit's packages in every enabled section that
/// cache has, so a section without a cache (dnf5 refuses the whole query for
/// one) is left out rather than failing it.
fn repoquery(f: &dyn Facts, inputs: &Inputs, limit: Duration) -> RepoQuery {
    use lmgw_machine::plan::embedded::{DNF5, RETIRED_TOOLKIT, TOOLKIT_BASE, TOOLKIT_FULL};
    if !inputs.dnf5 || inputs.ostree {
        return RepoQuery {
            skipped: Some("no dnf5 here".into()),
            ..Default::default()
        };
    }
    let cache = f
        .env("XDG_CACHE_HOME")
        .filter(|x| x.starts_with('/'))
        .map(PathBuf::from)
        .or_else(|| f.env("HOME").map(|h| Path::new(&h).join(".cache")))
        .map(|c| c.join("libdnf5"));
    let cached: Vec<String> = cache
        .and_then(|c| f.list(&c).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|n| cache_dir_id(n).map(str::to_string))
        .collect();
    let repos: Vec<String> = inputs
        .repos
        .sections
        .iter()
        .filter(|s| s.enabled && cached.contains(&s.id))
        .filter(|s| lmgw_machine::plan::RepoId::new(&s.id).is_some())
        .map(|s| format!("--repo={}", s.id))
        .collect();
    if repos.is_empty() {
        return RepoQuery {
            skipped: Some("your package metadata cache has none of the enabled sources".into()),
            ..Default::default()
        };
    }
    let mut args: Vec<&str> = vec!["repoquery", "-C", "--latest-limit=1"];
    args.extend(repos.iter().map(String::as_str));
    args.extend([
        "--queryformat",
        "%{name}\t%{evr}\t%{repoid}\t%{downloadsize}\n",
        TOOLKIT_BASE,
        TOOLKIT_FULL,
        RETIRED_TOOLKIT,
    ]);
    match f.run_limited(Path::new(DNF5), &args, limit) {
        Ok(out) if out.ok() => RepoQuery {
            rows: out
                .stdout
                .lines()
                .filter_map(|l| {
                    let c: Vec<&str> = l.split('\t').collect();
                    (c.len() == 4).then(|| {
                        (
                            c[0].to_string(),
                            c[1].to_string(),
                            c[2].to_string(),
                            c[3].trim().parse().ok(),
                        )
                    })
                })
                .collect(),
            skipped: None,
        },
        Ok(out) => RepoQuery {
            skipped: Some(format!(
                "dnf repoquery exited {}: {}",
                out.status_text(),
                out.stderr.trim()
            )),
            ..Default::default()
        },
        Err(e) => RepoQuery {
            skipped: Some(limited("dnf repoquery", &e, limit)),
            ..Default::default()
        },
    }
}

/// lmgw's own spec, judged from its files now; nothing written.
fn spec_facts(f: &dyn Facts, state: &AppState) -> SpecFacts {
    let cdi = state.cdi();
    let prefix = state.snapshot().settings.container_prefix.clone();
    let paths = cdi.paths(&prefix);
    let (now, stamp) = match (&paths, cdi.why_unmanaged()) {
        (Some(p), None) => {
            let stamp_bytes = std::fs::read(&p.stamp);
            let stamp = stamp_bytes
                .as_ref()
                .ok()
                .and_then(|b| serde_json::from_slice::<Stamp>(b).ok());
            let on_disk = OnDisk {
                spec: std::fs::read(&p.spec),
                stamp: stamp_bytes,
                spec_dir: std::fs::read_dir(&p.spec_dir).and_then(|d| {
                    let mut names = d
                        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
                        .collect::<std::io::Result<Vec<String>>>()?;
                    names.sort();
                    Ok(names)
                }),
            };
            (Some(judge(f, on_disk)), stamp)
        }
        _ => (None, None),
    };
    SpecFacts {
        default_dirs: cdi.default_dirs(),
        last: cdi.last(),
        paths,
        now,
        stamp,
    }
}

/// What a failed start's read leaves out of §1.2's facts (D96): what only
/// adds detail and download sizes.
const NOT_FOR_BLAME: &str = "not asked when a model's start failed: the check in the background \
                             asks";

/// §1.2's facts, all on the blocking thread. `full`: with `nvidia-ctk
/// --version` and the advisory `repoquery`, which only add detail and
/// download sizes, so a failed start's read leaves them out (D96).
fn toolkit_facts(f: &dyn Facts, state: &AppState, limit: Duration, full: bool) -> ToolkitFacts {
    // Through root's environment with the user's rights (`Facts::root_env`,
    // D87): the hash the person confirms is then one the helper computes.
    let inputs = Inputs::read(f, PROVIDERS);
    // In the server image the toolkit and its software source are the
    // image's (headless server design §7): no setup, no updates of its own
    // to ask the metadata cache about.
    let container = state.deployment().is_container();
    let ctk_version = if !full {
        Err(format!("nvidia-ctk --version {NOT_FOR_BLAME}"))
    } else {
        ctk_version(f, limit)
    };
    let dirs = lmgw_machine::cdi::configured_spec_dirs(f).dirs;
    let resolution = lmgw_machine::cdi::resolve(f, &dirs);
    let system_all = resolution
        .devices
        .get(&format!("{}=all", lmgw_machine::cdi::NVIDIA_KIND))
        .cloned();
    let frozen = (!container)
        .then(|| frozen(&inputs, &cache_times(f)))
        .flatten();
    let repoquery = if container {
        RepoQuery::default()
    } else if full {
        repoquery(f, &inputs, limit)
    } else {
        RepoQuery {
            skipped: Some(format!("dnf repoquery {NOT_FOR_BLAME}")),
            ..Default::default()
        }
    };
    ToolkitFacts {
        helper: if container {
            Err(super::container::NO_SETUP.into())
        } else {
            lmgw_machine::plan::instructions::helper_installed(f)
        },
        appimage: f.env("APPIMAGE").is_some_and(|a| !a.is_empty()),
        ctk_version,
        spec: spec_facts(f, state),
        system_dirs: dirs,
        system_all,
        frozen,
        repoquery,
        inputs,
        read_differently: state.machine.plans_read_differently(),
    }
}

/// `nvidia-ctk --version`'s version, or why it gave none.
fn ctk_version(f: &dyn Facts, limit: Duration) -> Result<String, String> {
    match f.run_limited(Path::new(NVIDIA_CTK), &["--version"], limit) {
        Ok(o) if o.ok() => o
            .stdout
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().last())
            .map(str::to_string)
            .ok_or_else(|| {
                format!(
                    "nvidia-ctk --version printed no version: {}",
                    o.stdout.trim()
                )
            }),
        Ok(o) => Err(format!(
            "nvidia-ctk --version exited {}: {}",
            o.status_text(),
            o.stderr.trim()
        )),
        Err(e) => Err(limited("nvidia-ctk --version", &e, limit)),
    }
}

/// One podman call within `limit`.
async fn podman(
    state: &AppState,
    args: &[&str],
    limit: Duration,
) -> Result<crate::runtime::registry::CmdOutput, std::io::Error> {
    let runner = state.runtime().runner();
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    match tokio::time::timeout(limit, runner.run("podman", &argv)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "`podman {}` did not answer within {} s, the load timeout ({})",
                args.join(" "),
                limit.as_secs(),
                crate::config::LOAD_TIMEOUT_LABEL
            ),
        )),
    }
}

/// `podman info`'s store and host facts (§1.3), within `limit`.
async fn podman_info(state: &AppState, limit: Duration) -> Result<PodmanInfo, String> {
    match podman(state, &["info", "--format", "json"], limit).await {
        Ok(o) if o.ok() => match serde_json::from_str::<serde_json::Value>(&o.stdout) {
            Ok(v) => Ok(PodmanInfo {
                rootless: v["host"]["security"]["rootless"].as_bool(),
                oci_runtime: v["host"]["ociRuntime"]["name"].as_str().map(str::to_string),
                cgroup_version: v["host"]["cgroupVersion"].as_str().map(str::to_string),
                graph_root: v["store"]["graphRoot"].as_str().map(str::to_string),
                graph_driver: v["store"]["graphDriverName"].as_str().map(str::to_string),
            }),
            Err(e) => Err(format!("podman info printed unreadable JSON: {e}")),
        },
        Ok(o) => Err(format!(
            "podman info exited {}: {}",
            o.status,
            o.stderr.trim()
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// The compressed layers Podman's image store has now, by short ID: what
/// an image download skips ([`images::store_layers_of`]), read for the
/// download estimate (find-a-model design D23) as §1.6 reads them.
pub(super) async fn layers_here(
    state: &AppState,
    f: Arc<dyn Facts>,
) -> Result<BTreeSet<String>, String> {
    let limit = crate::runtime::cdi::load_timeout(state);
    let info = podman_info(state, limit).await?;
    images::store_layers_of(f, Some(&info)).await
}

/// §1.3: `podman --version`, `podman --help`, `podman info`.
async fn podman_facts(state: &AppState, f: &Arc<dyn Facts>, limit: Duration) -> PodmanFacts {
    let container = state.deployment().is_container();
    let id_maps = if container {
        let f = f.clone();
        tokio::task::spawn_blocking(move || super::container::IdMaps::read(&*f))
            .await
            .ok()
    } else {
        None
    };
    let version = match podman(state, &["--version"], limit).await {
        Ok(o) if o.ok() => Ok(o.stdout.trim().to_string()),
        Ok(o) => Err(Some(format!(
            "podman --version exited {}: {}",
            o.status,
            o.stderr.trim()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(None),
        Err(e) => Err(Some(e.to_string())),
    };
    if matches!(version, Err(None)) {
        return PodmanFacts {
            version,
            has_option: None,
            info: Err("podman is not installed".into()),
            subids: None,
            id_maps,
        };
    }
    let has_option = match podman(state, &["--help"], limit).await {
        Ok(o) if o.ok() => Some(
            o.stdout.contains(crate::runtime::cdi::SPEC_DIR_OPTION)
                || o.stderr.contains(crate::runtime::cdi::SPEC_DIR_OPTION),
        ),
        _ => None,
    };
    let info = podman_info(state, limit).await;
    let subids = if info.is_err() && !container {
        let f = f.clone();
        tokio::task::spawn_blocking(move || subids(&*f))
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    PodmanFacts {
        version,
        has_option,
        info,
        subids,
        id_maps,
    }
}

/// Whether this user has ranges in `/etc/subuid` and `/etc/subgid`, by name
/// (from the user database, so an SSSD or LDAP account counts) or by UID.
fn subids(f: &dyn Facts) -> Option<(bool, bool)> {
    let uid = f.uid().map(|uid| uid.to_string());
    let name = f.user_name();
    let has = |file: &str| {
        lmgw_machine::facts::read_text(f, Path::new(file))
            .map(|t| {
                t.lines().any(|l| {
                    let who = l.split(':').next().unwrap_or("");
                    Some(who) == uid.as_deref() || Some(who) == name.as_deref()
                })
            })
            .unwrap_or(false)
    };
    Some((has("/etc/subuid"), has("/etc/subgid")))
}

/// Each class's defaults and each enabled row whose run arguments or image
/// are its own (§1.4).
pub(super) fn run_args(snap: &crate::config::Snapshot) -> (Vec<RunArgs>, Vec<RunArgs>) {
    let s = &snap.settings;
    let classes = vec![
        RunArgs {
            class: Class::Chat,
            model: None,
            args: s.router.extra_run_args.clone(),
            image: s.router.image.clone(),
        },
        RunArgs {
            class: Class::Aux,
            model: None,
            args: s.aux_router.extra_run_args.clone(),
            image: s.aux_router.image.clone(),
        },
        RunArgs {
            class: Class::Audio,
            model: None,
            args: s.audio.extra_run_args.clone(),
            image: s.audio.image.clone(),
        },
        RunArgs {
            class: Class::Image,
            model: None,
            args: s.image.extra_run_args.clone(),
            image: s.image.image.clone(),
        },
    ];
    let rows = crate::runtime::descriptor::model_runtimes(snap)
        .into_iter()
        .filter(|m| m.enabled)
        .filter_map(|m| {
            let class = classes.iter().find(|c| c.class == m.class)?;
            (m.extra_run_args != class.args || m.image != class.image).then(|| RunArgs {
                class: m.class,
                model: Some(m.model_id.clone()),
                args: m.extra_run_args.clone(),
                image: m.image.clone(),
            })
        })
        .collect();
    (classes, rows)
}

/// §1.1's and §1.2's facts on a blocking thread: NVML in its separate
/// process once the driver's halves agree, unless `skip_nvml` (a failed
/// start's read after its own write ran out of time on the card, D96);
/// §1.2's for an NVIDIA card whose driver works (NVML said so, or, with
/// NVML left out, the halves agree), `full` or as a failed start reads
/// them. `Err` when the blocking thread ended without an answer.
async fn card_and_toolkit(
    state: &Arc<AppState>,
    f: Arc<dyn Facts>,
    amd: super::AmdReader,
    skip_nvml: bool,
    full: bool,
) -> Result<(CardFacts, Option<ToolkitFacts>), String> {
    let load_timeout = crate::runtime::cdi::load_timeout(state);
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let mut card = card_facts(&*f);
        let decides = card.has(Vendor::Nvidia) && card.driver.nvml_decides();
        if decides && !skip_nvml {
            card.nvml = Some(probe::ask(&*f, load_timeout));
        }
        if card.has(Vendor::Amd) {
            card.amd = amd();
        }
        let nvidia_ok =
            matches!(card.nvml, Some(Ok(NvmlAnswer::Ok { .. }))) || (decides && skip_nvml);
        let toolkit = nvidia_ok.then(|| toolkit_facts(&*f, &st, load_timeout, full));
        (card, toolkit)
    })
    .await
    .map_err(|e| format!("reading the graphics card and its driver stopped without an answer: {e}"))
}

/// §1.1 and §1.2 alone, for a failed start's blame (D96, [`super::blame`]):
/// NVML left out when `skip_nvml`, §1.2 without what only adds detail.
pub(super) async fn for_blame(
    state: &Arc<AppState>,
    f: Arc<dyn Facts>,
    amd: super::AmdReader,
    skip_nvml: bool,
) -> Result<(CardFacts, Option<ToolkitFacts>), String> {
    card_and_toolkit(state, f, amd, skip_nvml, false).await
}

/// Read the machine. Blocking reads run on a blocking thread; podman and
/// the image list through the registry's runner. `Err` when the card's
/// reads ended without an answer.
pub(super) async fn gather(
    state: &Arc<AppState>,
    f: Arc<dyn Facts>,
    amd: super::AmdReader,
) -> Result<Gathered, String> {
    let load_timeout = crate::runtime::cdi::load_timeout(state);
    let (card, toolkit) = card_and_toolkit(state, f.clone(), amd, false, true).await?;
    let podman = podman_facts(state, &f, load_timeout).await;
    let snap = state.snapshot();
    let (classes, rows) = run_args(&snap);
    let s = &snap.settings;
    let mut places: Vec<(&'static str, String)> = vec![
        ("chat models", s.router.models_dir.clone()),
        (
            "embedding, rerank and decision models",
            s.aux_router.models_dir.clone(),
        ),
        ("audio models", s.audio.models_dir.clone()),
        ("image models", s.image.models_dir.clone()),
    ];
    if let Ok(info) = &podman.info {
        if let Some(root) = &info.graph_root {
            places.push((super::running::SOFTWARE, root.clone()));
        }
    }
    let deployment = state.deployment();
    if deployment.is_container() {
        // The image's third volume (§3.2): builds' scratch and the staging
        // copies `TMPDIR` points at.
        places.push((
            super::container::SCRATCH,
            super::container::SCRATCH_DIR.into(),
        ));
    }
    let f2 = f.clone();
    let container = deployment.is_container();
    let (rooms, mounts) = tokio::task::spawn_blocking(move || {
        let rooms = places
            .into_iter()
            .filter(|(_, p)| p.starts_with('/'))
            .map(|(what, path)| Room {
                what,
                space: f2.space(Path::new(&path)).map_err(|e| e.to_string()),
                path,
            })
            .collect();
        (
            rooms,
            container.then(|| super::container::read_mounts(&*f2)),
        )
    })
    .await
    .unwrap_or_default();

    let mut references: Vec<String> = classes
        .iter()
        .chain(rows.iter())
        .map(|r| r.image.clone())
        .filter(|i| !i.is_empty())
        .collect();
    references.sort();
    references.dedup();
    let images = if podman.version.is_ok() {
        images::images(
            state,
            &f,
            references,
            podman.info.as_ref().ok(),
            load_timeout,
        )
        .await
    } else {
        Images {
            images: Vec::new(),
            layers_here: Ok(BTreeSet::new()),
        }
    };
    let present = {
        use super::fit::present;
        let images = if podman.version.is_ok() {
            present::read_images(state, &snap).await
        } else {
            Default::default()
        };
        let running = present::running(state);
        present::Present {
            amd: present::amd_card(&card.pci, card.kfd, &card.render_nodes),
            images,
            kept: state.machine.kept_among(&running),
            running,
        }
    };
    Ok(Gathered {
        card,
        toolkit,
        podman,
        rooms,
        images,
        snap,
        present,
        deployment,
        mounts,
    })
}
