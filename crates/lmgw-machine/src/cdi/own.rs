//! lmgw's own CDI spec (machine-check design §3.6, ruling 4): the stamp that
//! records what it was generated for, and the judgement whether it is still
//! true of this machine.
//!
//! The judgement costs file reads, `stat`s and one directory listing: the
//! kernel module's version from `/proc`, the libraries' from one link (and a
//! `stat` of the file it points at), a `stat` of `nvidia-ctk` and of the rpm
//! database's files, the names of the `/dev/nvidia*` nodes, this boot's ID,
//! and a `stat` of every path the spec names. lmgw runs it before every GPU
//! container start.
//!
//! lmgw's spec is JSON (`nvidia-ctk cdi generate --format=json`), so this
//! judgement reads it with `serde_json` alone; the YAML reader is for the
//! system's specs (`super::system`).
//!
//! Generating the spec is lmgw-core's (`runtime::cdi`), because it writes
//! lmgw's own files; what makes a spec stale is decided here.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::spec::{Format, Spec};
use super::NVIDIA_KIND;
use crate::driver::{kernel_module, nvml_link, userspace, DriverState, Read};
use crate::facts::{read_text, Facts};

/// The spec's file name in lmgw's spec dir.
pub const OWN_SPEC_FILE: &str = "nvidia.json";

/// The stamp's file name. It lives beside the spec dir, never in it: Podman
/// reads every `*.json` in a spec dir as a spec.
pub const STAMP_FILE: &str = "nvidia.stamp.json";

/// The stamp's layout. A stamp of another schema is stale.
pub const STAMP_SCHEMA: u32 = 2;

/// The toolkit's generator.
pub const NVIDIA_CTK: &str = "/usr/bin/nvidia-ctk";

/// The hook the generated spec names, pinned on the generator's command line
/// (`--nvidia-cdi-hook-path`) instead of looked up in the caller's `PATH`:
/// the toolkit's own, beside [`NVIDIA_CTK`].
pub const NVIDIA_CDI_HOOK: &str = "/usr/bin/nvidia-cdi-hook";

/// The driver's setuid helper that creates device nodes (a Requires of
/// RPM Fusion's `xorg-x11-drv-nvidia`).
pub const NVIDIA_MODPROBE: &str = "/usr/bin/nvidia-modprobe";

/// The unified-memory device node CUDA needs. NVIDIA's own refresh creates it
/// before it generates; a spec generated without it lacks the node.
pub const NVIDIA_UVM: &str = "/dev/nvidia-uvm";

/// Where the NVIDIA device nodes are, each named `nvidia…`.
pub const DEV_DIR: &str = "/dev";

/// This boot's ID: a new one after every reboot. lmgw's spec dir survives a
/// reboot when it is under the data dir (no `XDG_RUNTIME_DIR`).
pub const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

/// A file's identity as `stat` gives it: a replaced or rewritten file has
/// another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: i128,
}

impl FileFingerprint {
    pub fn of(f: &dyn Facts, path: &Path) -> Option<Self> {
        f.stat(path).ok().map(|m| Self {
            ino: m.ino,
            len: m.len,
            mtime_ns: m.mtime_ns,
        })
    }
}

/// The generator a spec was written with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStamp {
    pub path: PathBuf,
    /// What `nvidia-ctk --version` said, for the detail (`1.20.1`).
    pub version: Option<String>,
    /// What the judgement compares: a toolkit update replaces the file.
    pub fingerprint: FileFingerprint,
}

/// What lmgw's spec was generated for (§3.6.1), beside the spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub schema: u32,
    /// The running kernel module's version; `None` when it could not be
    /// read (the spec is written anyway, and `nvidia-ctk` says what is
    /// wrong).
    pub kernel_module: Option<String>,
    /// The driver libraries' version; `None` when no `libnvidia-ml.so.1`
    /// was found or its name carries none.
    pub userspace: Option<String>,
    /// The file `libnvidia-ml.so.1` resolves to: an update replaces it even
    /// where its name says no version.
    pub nvml: Option<FileFingerprint>,
    pub nvidia_ctk: ToolStamp,
    /// The rpm database's last change ([`crate::rpm::database_changed`]),
    /// `None` on a machine without one.
    pub packages_changed_ns: Option<i128>,
    /// The `/dev/nvidia*` names, sorted: the spec names only the nodes that
    /// were there (a card plugged in later, `nvidia-uvm` created after a
    /// `nvidia-modprobe`).
    pub device_nodes: Vec<String>,
    /// [`BOOT_ID`] when it was written.
    pub boot_id: Option<String>,
    /// UTC, `YYYY-MM-DD HH:MM:SS`.
    pub written_at: String,
}

/// The machine as the judgement compares it with a stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub driver: DriverState,
    /// The module's version, when it can be read.
    pub kernel_module: Option<String>,
    /// The libraries' version, when it can be read.
    pub userspace: Option<String>,
    pub nvml: Option<FileFingerprint>,
    pub nvidia_ctk: Option<FileFingerprint>,
    pub packages_changed_ns: Option<i128>,
    pub device_nodes: Vec<String>,
    pub boot_id: Option<String>,
}

/// The `/dev/nvidia*` names, sorted (one listing of `/dev`).
pub fn device_nodes(f: &dyn Facts) -> Vec<String> {
    f.list(Path::new(DEV_DIR))
        .map(|names| {
            names
                .into_iter()
                .filter(|n| n.starts_with("nvidia"))
                .collect()
        })
        .unwrap_or_default()
}

impl Current {
    pub fn read(f: &dyn Facts) -> Self {
        let kernel = kernel_module(f);
        let user = userspace(f);
        let kernel_module = match &kernel {
            Read::Found(k) => Some(k.version.clone()),
            _ => None,
        };
        let userspace = match &user {
            Read::Found(u) => Some(u.version.clone()),
            _ => None,
        };
        Self {
            driver: DriverState::of(kernel, user),
            kernel_module,
            userspace,
            nvml: nvml_link(f).and_then(|link| FileFingerprint::of(f, &link)),
            nvidia_ctk: FileFingerprint::of(f, Path::new(NVIDIA_CTK)),
            packages_changed_ns: crate::rpm::database_changed(f).map(|(_, t)| t),
            device_nodes: device_nodes(f),
            boot_id: read_text(f, Path::new(BOOT_ID))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }

    /// Why the driver keeps a spec from being written (or used) now, if it
    /// does: a kernel module and userspace libraries that differ (a restart
    /// is pending), or no module at all.
    pub fn driver_block(&self) -> Option<Blocked> {
        blocked_by_driver(&self.driver)
    }

    /// The stamp a spec generated now gets: `None` without the toolkit, or
    /// while the driver keeps a spec from being written ([`Blocked`]).
    pub fn stamp(&self, nvidia_ctk_version: Option<String>, written_at: String) -> Option<Stamp> {
        if blocked_by_driver(&self.driver).is_some() {
            return None;
        }
        Some(Stamp {
            schema: STAMP_SCHEMA,
            kernel_module: self.kernel_module.clone(),
            userspace: self.userspace.clone(),
            nvml: self.nvml,
            nvidia_ctk: ToolStamp {
                path: PathBuf::from(NVIDIA_CTK),
                version: nvidia_ctk_version,
                fingerprint: self.nvidia_ctk?,
            },
            packages_changed_ns: self.packages_changed_ns,
            device_nodes: self.device_nodes.clone(),
            boot_id: self.boot_id.clone(),
            written_at,
        })
    }

    /// What differs between `self` and the machine `before` described, as
    /// the reasons a spec written for `before` is stale now: how a write
    /// finds the machine changed under it.
    pub fn changes_since(&self, before: &Current) -> Vec<Stale> {
        match before.stamp(None, String::new()) {
            Some(stamp) => compare(&stamp, self),
            None if before != self => vec![Stale::Machine],
            None => Vec::new(),
        }
    }
}

/// Why lmgw's spec has to be written again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stale {
    NoSpec,
    NoStamp,
    SpecUnreadable(String),
    StampUnreadable(String),
    /// Files in the spec dir beside the spec: Podman reads every `*.json`
    /// and `*.yaml` there, so they go when it is written again.
    ExtraFiles(Vec<String>),
    /// The spec dir could not be listed; why.
    SpecDirUnreadable(String),
    KernelModule {
        was: Option<String>,
        now: Option<String>,
    },
    Userspace {
        was: Option<String>,
        now: Option<String>,
    },
    /// The file `libnvidia-ml.so.1` resolves to was replaced, its version
    /// unchanged or unreadable.
    DriverLibrary,
    /// The toolkit's generator was replaced (an update) or removed.
    Toolkit {
        was: Option<String>,
    },
    /// A package transaction ran since the spec was written (D20).
    Packages,
    /// The `/dev/nvidia*` nodes are not the ones it was written for.
    DeviceNodes {
        was: Vec<String>,
        now: Vec<String>,
    },
    /// The computer was restarted since.
    Reboot,
    /// The machine changed in a way no stamp describes (the driver started
    /// or stopped blocking a write).
    Machine,
    /// Paths the spec names that are gone, each with what `stat` said.
    PathsGone(Vec<(PathBuf, String)>),
}

fn version_or_unreadable(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("unreadable")
}

impl fmt::Display for Stale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSpec => f.write_str("no spec written yet"),
            Self::NoStamp => f.write_str("no stamp beside the spec"),
            Self::SpecUnreadable(e) => write!(f, "the spec could not be read: {e}"),
            Self::StampUnreadable(e) => write!(f, "the stamp could not be read: {e}"),
            Self::ExtraFiles(names) => write!(
                f,
                "other files in its spec dir, which Podman would read too: {}",
                names.join(", ")
            ),
            Self::SpecDirUnreadable(e) => write!(f, "its spec dir could not be listed: {e}"),
            Self::KernelModule { was, now } => write!(
                f,
                "kernel module {} then, {} now",
                version_or_unreadable(was),
                version_or_unreadable(now)
            ),
            Self::Userspace { was, now } => write!(
                f,
                "driver libraries {} then, {} now",
                version_or_unreadable(was),
                version_or_unreadable(now)
            ),
            Self::DriverLibrary => f.write_str("the driver's libnvidia-ml was replaced"),
            Self::Toolkit { was } => match was {
                Some(v) => write!(f, "nvidia-ctk {v} was replaced or removed"),
                None => f.write_str("nvidia-ctk was replaced or removed"),
            },
            Self::Packages => f.write_str("packages were installed, updated or removed since"),
            Self::DeviceNodes { was, now } => write!(
                f,
                "the NVIDIA device nodes were {} then and are {} now",
                list_or_none(was),
                list_or_none(now)
            ),
            Self::Reboot => f.write_str("the computer was restarted since"),
            Self::Machine => f.write_str("the graphics driver changed"),
            Self::PathsGone(paths) => {
                let list: Vec<String> = paths
                    .iter()
                    .map(|(p, why)| format!("{} ({why})", p.display()))
                    .collect();
                write!(f, "paths it names are gone: {}", list.join(", "))
            }
        }
    }
}

fn list_or_none(names: &[String]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// Why lmgw cannot write its spec now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// The libraries were updated and the old kernel module still runs:
    /// nothing generated for either would work until a restart.
    Restart { kernel: String, userspace: String },
    /// No NVIDIA kernel module runs.
    NoModule,
    /// The spec has to be written and the toolkit's generator is not there.
    NoToolkit { stale: Vec<Stale> },
}

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Restart { kernel, userspace } => write!(
                f,
                "the running kernel module is {kernel} and the driver libraries on disk are \
                 {userspace}"
            ),
            Self::NoModule => write!(
                f,
                "{} is not there, so no NVIDIA kernel module is loaded",
                crate::driver::PROC_NVIDIA_VERSION
            ),
            Self::NoToolkit { .. } => write!(f, "{NVIDIA_CTK} is not there"),
        }
    }
}

/// What keeps the driver from having a spec written: the two halves at
/// different versions, or no module. A version that cannot be read does not:
/// `nvidia-ctk`'s own NVML start reports a real mismatch.
fn blocked_by_driver(driver: &DriverState) -> Option<Blocked> {
    match driver {
        DriverState::Differ { kernel, userspace } => Some(Blocked::Restart {
            kernel: kernel.version.clone(),
            userspace: userspace.version.clone(),
        }),
        DriverState::NoModule { .. } => Some(Blocked::NoModule),
        DriverState::Agree { .. }
        | DriverState::NoLibraries { .. }
        | DriverState::UserspaceUnreadable { .. }
        | DriverState::Unreadable(_) => None,
    }
}

/// The judgement of lmgw's spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    /// True of this machine: use it as it is.
    Fresh,
    /// Write it again, for these reasons.
    Stale(Vec<Stale>),
    /// It cannot be written now; keep what is there.
    Blocked(Blocked),
}

/// lmgw's own files as read from its spec dir: the spec's and the stamp's
/// bytes, and the names in the spec dir. lmgw-core reads them from the real
/// file system: they are lmgw's files, not facts about the machine.
pub struct OnDisk {
    pub spec: io::Result<Vec<u8>>,
    pub stamp: io::Result<Vec<u8>>,
    pub spec_dir: io::Result<Vec<String>>,
}

/// Every path `spec` names that `stat` cannot find, with why.
pub fn paths_gone(f: &dyn Facts, spec: &Spec) -> Vec<(PathBuf, String)> {
    spec.named_paths()
        .into_iter()
        .filter_map(|p| match f.stat(&p) {
            Ok(_) => None,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Some((p, "not there".into())),
            Err(e) => Some((p, e.to_string())),
        })
        .collect()
}

/// Judge lmgw's spec as `on_disk` holds it against this machine.
///
/// The driver comes first: while its two halves differ (or no module runs),
/// the spec is never written, whatever it says. Then every reason it is stale,
/// all of them, so the log names each. A stale spec without a toolkit to write
/// it again is [`Blocked::NoToolkit`].
pub fn judge(f: &dyn Facts, on_disk: OnDisk) -> Judgement {
    judge_against(f, &Current::read(f), on_disk)
}

/// [`judge`] against facts the caller already read.
pub fn judge_against(f: &dyn Facts, current: &Current, on_disk: OnDisk) -> Judgement {
    if let Some(blocked) = blocked_by_driver(&current.driver) {
        return Judgement::Blocked(blocked);
    }
    let mut stale = Vec::new();
    match on_disk.spec {
        Err(e) if e.kind() == io::ErrorKind::NotFound => stale.push(Stale::NoSpec),
        Err(e) => stale.push(Stale::SpecUnreadable(e.to_string())),
        Ok(bytes) => match Spec::parse(&bytes, Format::Json) {
            Err(e) => stale.push(Stale::SpecUnreadable(e.to_string())),
            Ok(s) if s.kind != NVIDIA_KIND => stale.push(Stale::SpecUnreadable(format!(
                "it defines '{}', not {NVIDIA_KIND}",
                s.kind
            ))),
            Ok(s) => {
                let gone = paths_gone(f, &s);
                if !gone.is_empty() {
                    stale.push(Stale::PathsGone(gone));
                }
            }
        },
    }
    match on_disk.spec_dir {
        Ok(names) => {
            let extra: Vec<String> = names.into_iter().filter(|n| n != OWN_SPEC_FILE).collect();
            if !extra.is_empty() {
                stale.push(Stale::ExtraFiles(extra));
            }
        }
        // No dir: no spec either, which says so already.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => stale.push(Stale::SpecDirUnreadable(e.to_string())),
    }
    match on_disk.stamp {
        Err(e) if e.kind() == io::ErrorKind::NotFound => stale.push(Stale::NoStamp),
        Err(e) => stale.push(Stale::StampUnreadable(e.to_string())),
        Ok(bytes) => match serde_json::from_slice::<Stamp>(&bytes) {
            Ok(s) if s.schema == STAMP_SCHEMA => stale.extend(compare(&s, current)),
            Ok(s) => stale.push(Stale::StampUnreadable(format!(
                "it is of layout {}, this lmgw writes {STAMP_SCHEMA}",
                s.schema
            ))),
            // An older layout may not parse as this one at all: say which
            // layout it is when it says.
            Err(e) => stale.push(Stale::StampUnreadable(
                match serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|v| v.get("schema").and_then(|s| s.as_u64()))
                {
                    Some(schema) if schema != u64::from(STAMP_SCHEMA) => {
                        format!("it is of layout {schema}, this lmgw writes {STAMP_SCHEMA}")
                    }
                    _ => e.to_string(),
                },
            )),
        },
    }
    if stale.is_empty() {
        Judgement::Fresh
    } else if current.nvidia_ctk.is_none() {
        Judgement::Blocked(Blocked::NoToolkit { stale })
    } else {
        Judgement::Stale(stale)
    }
}

/// Every way `now` differs from what `stamp` was written for.
fn compare(stamp: &Stamp, now: &Current) -> Vec<Stale> {
    let mut stale = Vec::new();
    if stamp.kernel_module != now.kernel_module {
        stale.push(Stale::KernelModule {
            was: stamp.kernel_module.clone(),
            now: now.kernel_module.clone(),
        });
    }
    if stamp.userspace != now.userspace {
        stale.push(Stale::Userspace {
            was: stamp.userspace.clone(),
            now: now.userspace.clone(),
        });
    } else if stamp.nvml != now.nvml {
        stale.push(Stale::DriverLibrary);
    }
    if now.nvidia_ctk != Some(stamp.nvidia_ctk.fingerprint) {
        stale.push(Stale::Toolkit {
            was: stamp.nvidia_ctk.version.clone(),
        });
    }
    if now.packages_changed_ns != stamp.packages_changed_ns {
        stale.push(Stale::Packages);
    }
    if now.device_nodes != stamp.device_nodes {
        stale.push(Stale::DeviceNodes {
            was: stamp.device_nodes.clone(),
            now: now.device_nodes.clone(),
        });
    }
    if now.boot_id != stamp.boot_id {
        stale.push(Stale::Reboot);
    }
    stale
}
