//! The closed list of root steps (machine-check design §3.2): S1 or S1′,
//! S2, then one of S4, S4′ and S4″. Every program is an absolute path, every
//! argument a constant, except the package names and the section id, which
//! come from the machine's rpm database and repository files and are checked
//! to be names (never an option) before a step can hold them.

use crate::rpm::RPMKEYS;

use super::embedded::{
    DNF5, KEY_FILE, KEY_URL, NVIDIA_KEY, REPO_FILE, REPO_TEXT, RPM_OSTREE, TOOLKIT_BASE,
};

/// A package name as rpm allows it, and never something dnf would read as an
/// option: letters, digits and `._+-`, not starting with `-` or `.`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageName(String);

impl PackageName {
    pub fn new(name: &str) -> Option<Self> {
        let first_ok = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '+');
        let rest_ok = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
        (first_ok && rest_ok).then(|| Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A repository section id, never an option: letters, digits and `._:-`,
/// not starting with `-`. A `.` in it is safe in `--setopt=<id>.<option>`
/// and `config-manager setopt <id>.<option>`: dnf5 splits the key at its
/// *last* `.` (`key.rfind('.')` in `dnf5/main.cpp`'s setopt hook,
/// `Context::Impl::apply_repository_setopts` in `dnf5/context.cpp` and
/// `dnf5-plugins/config-manager_plugin/setopt.cpp`, read on dnf5's `main`
/// 2026-11-03), and option names have none. It matches the id part as a
/// glob (`QueryCmp::GLOB`), which is why `*`, `?` and `[` are not allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoId(String);

impl RepoId {
    pub fn new(id: &str) -> Option<Self> {
        let first_ok = id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let rest_ok = id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
        (first_ok && rest_ok).then(|| Self(id.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A list with at least one element: a first one, then the rest. What S4′'s
/// package list is, so an upgrade without a package — a bare `dnf upgrade
/// -y`, which would upgrade the whole system as root — cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T> {
    pub first: T,
    pub rest: Vec<T>,
}

impl<T> NonEmpty<T> {
    pub fn new(first: T, rest: Vec<T>) -> Self {
        Self { first, rest }
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        std::iter::once(&self.first).chain(self.rest.iter())
    }
}

/// `<scope>` (§3.2): the transaction reads one section only, fails loudly
/// when it cannot load, and checks package signatures against the pinned
/// key — even in a section the person wrote with `gpgcheck=0`, without
/// editing their file. A provider-table section keeps the distribution's own
/// keys: it gets the first two only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub repo: RepoId,
    /// Package checks against lmgw's copy of NVIDIA's key.
    pub nvidia_key: bool,
    /// Packages the section's `includepkgs` does not let through that this
    /// transaction needs (S4′'s other installed toolkit packages through
    /// lmgw's own section): appended to the list for this one command, as
    /// `--setopt=<id>.includepkgs=` does in dnf5 (an append option,
    /// `OptionStringAppendList` in `libdnf5/repo/config_repo.cpp`). Empty
    /// for every other step.
    pub include: Vec<PackageName>,
}

impl Scope {
    pub fn args(&self) -> Vec<String> {
        let id = self.repo.as_str();
        let mut out = vec![
            format!("--repo={id}"),
            format!("--setopt={id}.skip_if_unavailable=False"),
        ];
        if self.nvidia_key {
            out.push(format!("--setopt={id}.gpgcheck=1"));
            out.push(format!("--setopt={id}.gpgkey={KEY_URL}"));
        }
        for p in &self.include {
            out.push(format!("--setopt={id}.includepkgs={}", p.as_str()));
        }
        out.push("--setopt=install_weak_deps=False".into());
        out
    }
}

/// How S4 installs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installer {
    /// `dnf install`, scoped.
    Dnf(Scope),
    /// `rpm-ostree install` on an image-based system, then a restart. It has
    /// no `--repo`: it reads the sections as they are.
    RpmOstree,
}

/// A file a step writes: always created with `O_CREAT|O_EXCL` (a new file),
/// or, when `replace`, lmgw's own file replaced through a temporary file in
/// the same directory (§3.2, "What S1 writes, and how").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileWrite {
    pub path: &'static str,
    pub content: &'static str,
    pub mode: u32,
    pub replace: bool,
}

/// Which step it is: the plan's and the API's name for it. The root
/// helper's lines ([`crate::helper::Event`]) carry the same name as
/// [`Self::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StepKind {
    /// S1: add (or replace) lmgw's repository file.
    #[serde(rename = "s1_add_repo")]
    AddRepo,
    /// S1′: switch a section on that is switched off.
    #[serde(rename = "s1_enable_repo")]
    EnableRepo,
    /// S2: add NVIDIA's key.
    #[serde(rename = "s2_add_key")]
    AddKey,
    /// S4: install the toolkit.
    #[serde(rename = "s4_install")]
    Install,
    /// S4′: update NVIDIA's toolkit.
    #[serde(rename = "s4_update")]
    Update,
    /// S4″: switch from Fedora's retired package.
    #[serde(rename = "s4_swap")]
    Swap,
}

impl StepKind {
    /// The name the hash and the API use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AddRepo => "s1_add_repo",
            Self::EnableRepo => "s1_enable_repo",
            Self::AddKey => "s2_add_key",
            Self::Install => "s4_install",
            Self::Update => "s4_update",
            Self::Swap => "s4_swap",
        }
    }
}

/// One root step. The list is closed: there is no step that runs anything
/// else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// S1: create `/etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo`, or
    /// replace lmgw's older text there.
    AddRepo { write: FileWrite },
    /// S1′: `dnf config-manager setopt <id>.enabled=1`, which writes dnf's
    /// own override file and leaves the person's `.repo` file as it is.
    EnableRepo { repo: RepoId },
    /// S2: lmgw's copy of NVIDIA's key (`write`, when it is not there with
    /// this text yet) and its import into rpm's keys (`import`, when rpm does
    /// not trust it yet). At least one of the two.
    AddKey {
        write: Option<FileWrite>,
        import: bool,
    },
    /// S4: install the toolkit.
    Install {
        via: Installer,
        package: PackageName,
    },
    /// S4′: update NVIDIA's toolkit: its owner package first, then every
    /// other installed package of [`super::embedded::TOOLKIT_FAMILY`].
    Update {
        scope: Scope,
        packages: NonEmpty<PackageName>,
    },
    /// S4″: replace Fedora's retired package with NVIDIA's.
    Swap {
        scope: Scope,
        from: PackageName,
        to: PackageName,
    },
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

impl Step {
    /// S1, writing [`REPO_TEXT`].
    pub fn add_repo(replace: bool) -> Self {
        Self::AddRepo {
            write: FileWrite {
                path: REPO_FILE,
                content: REPO_TEXT,
                mode: 0o644,
                replace,
            },
        }
    }

    /// S2: `write` the key file (`Some(replace)`) and/or `import` it.
    pub fn add_key(write: Option<bool>, import: bool) -> Self {
        Self::AddKey {
            write: write.map(|replace| FileWrite {
                path: KEY_FILE,
                content: NVIDIA_KEY,
                mode: 0o644,
                replace,
            }),
            import,
        }
    }

    pub fn kind(&self) -> StepKind {
        match self {
            Self::AddRepo { .. } => StepKind::AddRepo,
            Self::EnableRepo { .. } => StepKind::EnableRepo,
            Self::AddKey { .. } => StepKind::AddKey,
            Self::Install { .. } => StepKind::Install,
            Self::Update { .. } => StepKind::Update,
            Self::Swap { .. } => StepKind::Swap,
        }
    }

    /// The file it writes, if any.
    pub fn file(&self) -> Option<&FileWrite> {
        match self {
            Self::AddRepo { write } => Some(write),
            Self::AddKey { write, .. } => write.as_ref(),
            _ => None,
        }
    }

    /// The programs it runs as root, each an exact argv whose first element
    /// is an absolute path, in order.
    pub fn commands(&self) -> Vec<Vec<String>> {
        match self {
            Self::AddRepo { .. } => Vec::new(),
            Self::EnableRepo { repo } => vec![args(&[
                DNF5,
                "config-manager",
                "setopt",
                &format!("{}.enabled=1", repo.as_str()),
            ])],
            Self::AddKey { import, .. } => {
                if *import {
                    vec![args(&[RPMKEYS, "--import", KEY_FILE])]
                } else {
                    Vec::new()
                }
            }
            Self::Install { via, package } => match via {
                Installer::Dnf(scope) => {
                    let mut argv = args(&[DNF5, "install", "-y"]);
                    argv.extend(scope.args());
                    argv.push(package.as_str().to_string());
                    vec![argv]
                }
                Installer::RpmOstree => {
                    vec![args(&[RPM_OSTREE, "install", package.as_str()])]
                }
            },
            Self::Update { scope, packages } => {
                let mut argv = args(&[DNF5, "upgrade", "-y"]);
                argv.extend(scope.args());
                argv.extend(packages.iter().map(|p| p.as_str().to_string()));
                vec![argv]
            }
            Self::Swap { scope, from, to } => {
                let mut argv = args(&[DNF5, "swap", "-y"]);
                argv.extend(scope.args());
                argv.push(from.as_str().to_string());
                argv.push(to.as_str().to_string());
                vec![argv]
            }
        }
    }

    /// The package names a dnf or rpm-ostree command of this step ends with,
    /// in order (empty for the steps that install nothing).
    pub fn packages(&self) -> Vec<&str> {
        match self {
            Self::Install { package, .. } => vec![package.as_str()],
            Self::Update { packages, .. } => packages.iter().map(PackageName::as_str).collect(),
            Self::Swap { from, to, .. } => vec![from.as_str(), to.as_str()],
            _ => Vec::new(),
        }
    }

    /// The bullet the confirmation shows for it: plain words, the technical
    /// names only where the person has to recognise a package.
    pub fn sentence(&self) -> String {
        match self {
            Self::AddRepo { write } if write.replace => {
                "Update lmgw's entry for NVIDIA's software source to the text this version of \
                 lmgw writes"
                    .into()
            }
            Self::AddRepo { .. } => {
                "Add NVIDIA's software source (nvidia.github.io), limited to the one piece \
                 below, so that piece gets updates with the rest of your system"
                    .into()
            }
            Self::EnableRepo { .. } => {
                "Switch on NVIDIA's software source, which is on this computer but switched off"
                    .into()
            }
            Self::AddKey { import: true, .. } => {
                "Add NVIDIA's signing key, so your computer checks that the piece below really \
                 comes from NVIDIA"
                    .into()
            }
            Self::AddKey { .. } => {
                "Save NVIDIA's signing key where lmgw's checks of NVIDIA's packages look for it"
                    .into()
            }
            Self::Install { via, .. } => match via {
                Installer::Dnf(_) => {
                    "Install NVIDIA's tool that lets models use the graphics card".into()
                }
                Installer::RpmOstree => {
                    "Install NVIDIA's tool that lets models use the graphics card, then restart \
                     the computer"
                        .into()
                }
            },
            Self::Update { .. } => {
                "Update NVIDIA's tool that lets models use the graphics card".into()
            }
            Self::Swap { from, .. } => format!(
                "Replace the piece Fedora stopped updating ({}) with NVIDIA's current one",
                from.as_str()
            ),
        }
    }
}

/// [`TOOLKIT_BASE`] as a package name.
pub fn toolkit_base() -> PackageName {
    PackageName::new(TOOLKIT_BASE).expect("a constant package name")
}
