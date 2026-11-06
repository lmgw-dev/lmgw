//! The NVIDIA driver's two halves and Secure Boot (machine-check design §1.1).
//!
//! **Why versions, and no NVML here.** A process that has loaded
//! `libnvidia-ml` keeps the old library mapped after a driver update, and that
//! old library matches the old kernel module, which runs until the next boot:
//! an in-process NVML call answers as if nothing changed. So the driver is read
//! as two versions — the running kernel module's from `/proc`, the userspace
//! libraries' from the file `libnvidia-ml.so.1` points at on disk — and a
//! difference between them is "a driver update waiting for a restart". NVML
//! itself is read in a separate process, by the check (WP4), and only once the
//! two agree.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::facts::{read_text, Facts, Kind};

/// The running kernel module's own description.
pub const PROC_NVIDIA_VERSION: &str = "/proc/driver/nvidia/version";

/// The NVML library's soname link, the userspace driver's version carrier.
pub const NVML_SONAME: &str = "libnvidia-ml.so.1";

/// Where [`NVML_SONAME`] is looked for, in this order: Fedora's and RHEL's
/// 64-bit library dir, Debian's and Ubuntu's multiarch dirs, the plain
/// `/usr/lib` Arch uses, then NixOS's driver dir (`hardware.graphics` links
/// the driver's libraries there).
pub const NVML_LIB_DIRS: [&str; 5] = [
    "/usr/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/lib",
    "/run/opengl-driver/lib",
];

/// Secure Boot's EFI variable (global variable GUID), world-readable in
/// efivarfs on Fedora: four attribute bytes, then one data byte.
pub const SECURE_BOOT_VAR: &str =
    "/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c";

/// Present on a machine booted through EFI.
pub const EFI_DIR: &str = "/sys/firmware/efi";

/// Which kernel module NVIDIA ships: the open one (GPL, Turing and newer) or
/// the proprietary one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleFlavor {
    Open,
    Proprietary,
}

/// The running NVIDIA kernel module, from [`PROC_NVIDIA_VERSION`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelModule {
    pub version: String,
    pub flavor: ModuleFlavor,
    /// The `NVRM version:` line as the module wrote it, for the detail.
    pub line: String,
}

/// The userspace driver on disk, from [`NVML_SONAME`]'s link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Userspace {
    /// The soname link that was found, e.g. `/usr/lib64/libnvidia-ml.so.1`.
    pub link: PathBuf,
    /// Where it points, every link resolved, e.g.
    /// `/usr/lib64/libnvidia-ml.so.600.20.03`.
    pub target: PathBuf,
    pub version: String,
}

/// Is `s` a dotted number, as driver versions are (`600.20.03`, `550.54`)?
fn is_version(s: &str) -> bool {
    let mut parts = 0;
    for p in s.split('.') {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts >= 2
}

/// Read the module's line out of [`PROC_NVIDIA_VERSION`]'s text. Both modules
/// write `NVRM version: NVIDIA UNIX … Kernel Module … <version> …`; the open
/// one says `Open Kernel Module`. The version is the first dotted number on
/// the line. `None` when no such line or number is there.
pub fn parse_nvrm(text: &str) -> Option<KernelModule> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("NVRM version:"))?
        .trim();
    let version = line.split_whitespace().find(|t| is_version(t))?.to_string();
    let flavor = if line.contains("Open Kernel Module") {
        ModuleFlavor::Open
    } else {
        ModuleFlavor::Proprietary
    };
    Some(KernelModule {
        version,
        flavor,
        line: line.to_string(),
    })
}

/// What reading one half of the driver found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read<T> {
    Found(T),
    /// Nothing there: no module loaded, no library installed.
    Absent,
    /// Something there that could not be read; the why.
    Unreadable(String),
}

/// The running kernel module.
pub fn kernel_module(f: &dyn Facts) -> Read<KernelModule> {
    match read_text(f, Path::new(PROC_NVIDIA_VERSION)) {
        Ok(text) => match parse_nvrm(&text) {
            Some(m) => Read::Found(m),
            None => Read::Unreadable(format!(
                "{PROC_NVIDIA_VERSION} names no driver version: {}",
                text.lines().next().unwrap_or("").trim()
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Read::Absent,
        Err(e) => Read::Unreadable(format!("{PROC_NVIDIA_VERSION}: {e}")),
    }
}

/// The version a library file name carries: `libnvidia-ml.so.600.20.03` →
/// `600.20.03`.
pub fn version_of_library(target: &Path) -> Option<String> {
    let name = target.file_name()?.to_str()?;
    let v = name.strip_prefix("libnvidia-ml.so.")?;
    is_version(v).then(|| v.to_string())
}

/// The first [`NVML_SONAME`] in [`NVML_LIB_DIRS`] that is there (a link or
/// a file), whatever it points at.
pub fn nvml_link(f: &dyn Facts) -> Option<PathBuf> {
    NVML_LIB_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(NVML_SONAME))
        .find(|link| f.lstat(link).is_ok())
}

/// The userspace driver: the first [`NVML_SONAME`] in [`NVML_LIB_DIRS`], and
/// the version of the file it resolves to.
pub fn userspace(f: &dyn Facts) -> Read<Userspace> {
    if let Some(link) = nvml_link(f) {
        let target = match f.canonicalize(&link) {
            Ok(t) => t,
            Err(e) => return Read::Unreadable(format!("{} points nowhere: {e}", link.display())),
        };
        return match version_of_library(&target) {
            Some(version) => Read::Found(Userspace {
                link,
                target,
                version,
            }),
            None => Read::Unreadable(format!(
                "{} points at {}, whose name carries no driver version",
                link.display(),
                target.display()
            )),
        };
    }
    Read::Absent
}

/// The driver as a whole: do the running module and the libraries on disk
/// agree?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverState {
    /// Both there, one version.
    Agree {
        kernel: KernelModule,
        userspace: Userspace,
    },
    /// Both there, different versions: the libraries were updated and the old
    /// module still runs. Nothing works right until a restart.
    Differ {
        kernel: KernelModule,
        userspace: Userspace,
    },
    /// No NVIDIA kernel module is running (not installed, not built for this
    /// kernel yet, or kept from loading).
    NoModule { userspace: Read<Userspace> },
    /// The module runs, and no NVML library is where lmgw looks.
    NoLibraries { kernel: KernelModule },
    /// The module runs, and the libraries' version cannot be read from the
    /// file [`NVML_SONAME`] points at (an unversioned name, a regular file,
    /// a link that points nowhere): NVML in a separate process says whether
    /// the two match (its `version_mismatch`), so the check asks it and
    /// lmgw's spec is written as on [`Self::Agree`].
    UserspaceUnreadable { kernel: KernelModule, why: String },
    /// The kernel module's own description could not be read; the why.
    Unreadable(String),
}

impl DriverState {
    pub fn read(f: &dyn Facts) -> Self {
        Self::of(kernel_module(f), userspace(f))
    }

    /// The driver from its two halves as [`kernel_module`] and [`userspace`]
    /// read them.
    pub fn of(kernel: Read<KernelModule>, userspace: Read<Userspace>) -> Self {
        match (kernel, userspace) {
            (Read::Found(kernel), Read::Found(userspace)) => {
                if kernel.version == userspace.version {
                    Self::Agree { kernel, userspace }
                } else {
                    Self::Differ { kernel, userspace }
                }
            }
            (Read::Absent, userspace) => Self::NoModule { userspace },
            (Read::Found(kernel), Read::Absent) => Self::NoLibraries { kernel },
            (Read::Found(kernel), Read::Unreadable(why)) => {
                Self::UserspaceUnreadable { kernel, why }
            }
            (Read::Unreadable(why), _) => Self::Unreadable(why),
        }
    }

    /// Whether NVML in a separate process is the one to say if the driver
    /// works: the module runs and the libraries agree with it, or their
    /// version cannot be read and NVML's own start compares them.
    pub fn nvml_decides(&self) -> bool {
        matches!(self, Self::Agree { .. } | Self::UserspaceUnreadable { .. })
    }

    /// The one driver version, when the two halves agree.
    pub fn agreed_version(&self) -> Option<&str> {
        match self {
            Self::Agree { kernel, .. } => Some(&kernel.version),
            _ => None,
        }
    }
}

/// Where the kernels' modules are: `<dir>/<release>/…`.
pub const MODULES_DIR: &str = "/lib/modules";

/// The running kernel's release, as `uname -r` prints it.
pub const OSRELEASE: &str = "/proc/sys/kernel/osrelease";

/// The kernel's statistics, whose `btime` line is when this boot started.
pub const PROC_STAT: &str = "/proc/stat";

/// Where a built NVIDIA module is put under `<MODULES_DIR>/<release>/`:
/// akmods' (RPM Fusion's `akmod-nvidia`, `extra/nvidia` on Fedora 44), DKMS's
/// and a distribution's packaged module.
pub const MODULE_SUBDIRS: [&str; 5] = [
    "extra/nvidia",
    "extra",
    "updates/dkms",
    "updates",
    "kernel/drivers/video",
];

/// The NVIDIA kernel module built for the running kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltModule {
    pub release: String,
    pub path: PathBuf,
    /// When it was put there: its change time. rpm sets an installed file's
    /// modification time to the package's (an akmods-built `nvidia.ko.xz`
    /// carries its source's release date), so only the change time says when
    /// it arrived.
    pub installed_ns: i128,
}

/// Whether an NVIDIA kernel module is built for the running kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleFile {
    Built(BuiltModule),
    /// None in [`MODULE_SUBDIRS`] of the running kernel (yet).
    NotBuilt {
        release: String,
    },
    /// The running kernel's release could not be read; the why.
    Unknown(String),
}

/// Is `name` the NVIDIA module's file: `nvidia.ko`, or it compressed
/// (`nvidia.ko.xz`, `.zst`, `.gz`)?
fn is_nvidia_ko(name: &str) -> bool {
    name == "nvidia.ko" || name.starts_with("nvidia.ko.")
}

/// The NVIDIA module built for the running kernel ([`OSRELEASE`]), looked
/// for in [`MODULE_SUBDIRS`] in order.
pub fn module_file(f: &dyn Facts) -> ModuleFile {
    let release = match read_text(f, Path::new(OSRELEASE)) {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        Ok(_) => return ModuleFile::Unknown(format!("{OSRELEASE} is empty")),
        Err(e) => return ModuleFile::Unknown(format!("{OSRELEASE}: {e}")),
    };
    let base = Path::new(MODULES_DIR).join(&release);
    for sub in MODULE_SUBDIRS {
        let dir = base.join(sub);
        let Ok(names) = f.list(&dir) else { continue };
        for name in names.iter().filter(|n| is_nvidia_ko(n)) {
            let path = dir.join(name);
            if let Ok(m) = f.stat(&path) {
                if m.kind == Kind::File {
                    return ModuleFile::Built(BuiltModule {
                        release,
                        path,
                        installed_ns: m.ctime_ns,
                    });
                }
            }
        }
    }
    ModuleFile::NotBuilt { release }
}

/// When this boot started, from [`PROC_STAT`]'s `btime` (seconds since the
/// epoch), in nanoseconds; `None` when it cannot be read.
pub fn boot_time_ns(f: &dyn Facts) -> Option<i128> {
    let text = read_text(f, Path::new(PROC_STAT)).ok()?;
    let secs: i128 = text
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(secs * 1_000_000_000)
}

/// Secure Boot, as the firmware reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecureBoot {
    On,
    Off,
    /// Not booted through EFI: there is no Secure Boot to speak of.
    NotEfi,
    /// EFI, and the variable could not be read; the why.
    Unknown(String),
}

/// Secure Boot's state from [`SECURE_BOOT_VAR`].
pub fn secure_boot(f: &dyn Facts) -> SecureBoot {
    match f.read(Path::new(SECURE_BOOT_VAR)) {
        Ok(bytes) => match bytes.get(4) {
            Some(1) => SecureBoot::On,
            Some(0) => SecureBoot::Off,
            Some(other) => {
                SecureBoot::Unknown(format!("{SECURE_BOOT_VAR} holds {other}, neither 0 nor 1"))
            }
            None => SecureBoot::Unknown(format!(
                "{SECURE_BOOT_VAR} is {} bytes long, too short to hold a value",
                bytes.len()
            )),
        },
        Err(_) if !matches!(f.stat(Path::new(EFI_DIR)), Ok(m) if m.kind == Kind::Dir) => {
            SecureBoot::NotEfi
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SecureBoot::Off,
        Err(e) => SecureBoot::Unknown(format!("{SECURE_BOOT_VAR}: {e}")),
    }
}
