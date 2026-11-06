//! The plan (machine-check design §3.2, WP2): the closed list of root steps
//! that gets the graphics card's container toolkit onto this computer.
//!
//! Computed here, once, for every place that shows or runs it: the check's
//! plan block and commands fold, the instructions of §3.7 and, as root, the
//! helper (WP6). Root computes it from facts it reads exactly as the user
//! does ([`Inputs::read`]): the rpm database (`rpm -qf`, `rpm -q`, header
//! signature key IDs, `rpmkeys --list`), the world-readable repository files
//! with dnf5's overrides, `/etc/os-release`, whether `/run/ostree-booted`,
//! `/usr/bin/dnf` and `/usr/bin/dnf5` are there, lmgw's own two files and
//! the embedded repository text and key. Nothing else: not the CDI specs,
//! NVML, dnf's metadata caches, the user's environment or lmgw's version.
//!
//! One input is not root's to read: whether lmgw's own spec generation
//! failed, which is what offers S4′ ([`Want::update_toolkit`]). It is a
//! single flag, and S4′ only updates the toolkit's own packages, scoped to
//! one section; so root computes the plan for both of its values and accepts
//! either hash ([`accepted_hashes`]).
//!
//! - [`embedded`]: the repository file, NVIDIA's key, the paths;
//! - [`toolkit`]: who owns `nvidia-ctk` (D24);
//! - [`sections`]: the provider table, lmgw's own files, the section chosen;
//! - [`steps`]: S1, S1′, S2, S4, S4′, S4″, `<scope>`, `NonEmpty`;
//! - [`hash`]: [`HashedPlan`] and its schema;
//! - [`instructions`]: §3.7's systems, the button or commands to type;
//! - [`scripts`]: what a step's package changes by itself (display only).
//!
//! **Root reads through [`crate::SystemFacts::for_root`].** The facts above
//! are the same for root and the user only when nothing in root's
//! environment changes them: rpm reads `$HOME/.rpmmacros` (where `%_dbpath`
//! can point at another database), dnf and rpm read the locale, and a
//! program found in `PATH` could be anything. The helper (WP6) builds its
//! facts with that constructor, which starts every program from an empty
//! environment with a fixed `HOME=/root`, `PATH` and `LC_ALL`, in `/`. The
//! user's side reads [`Inputs`] through the same environment
//! ([`crate::Facts::root_env`], D87): otherwise a `~/.rpmmacros` the user
//! writes would make every hash the person confirms one root refuses.

use std::fmt;
use std::path::Path;

use crate::facts::{exists, Facts};
use crate::os::{os_release, ostree_booted, OsRelease};
use crate::repos::{read_repos, Repos};
use crate::rpm::{imported_keys, owner_of, query, ImportedKeys};

pub mod embedded;
pub mod hash;
pub mod instructions;
pub mod scripts;
pub mod sections;
pub mod steps;
pub mod toolkit;

pub use hash::{HashedPlan, PLAN_SCHEMA};
pub use instructions::{how, terminal_lines, How, UserSide};
pub use scripts::package_scripts;
pub use sections::{Origin, OwnFile, Provider, Section, PROVIDERS};
pub use steps::{Installer, NonEmpty, PackageName, RepoId, Scope, Step, StepKind};
pub use toolkit::Toolkit;

use embedded::{DNF, DNF5, NVIDIA_KEY_FINGERPRINT, TOOLKIT_BASE, TOOLKIT_FAMILY};

/// The toolkit's generator, whose owner every toolkit step keys on.
pub const NVIDIA_CTK: &str = crate::cdi::own::NVIDIA_CTK;

/// Which package manager the plan's commands are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    /// dnf5 (Fedora 41 and later): the plan's own commands.
    Dnf5,
    /// A dnf older than dnf5 (RHEL 9 and its kin).
    OtherDnf,
    /// An image-based system (`/run/ostree-booted`): rpm-ostree.
    Ostree,
    /// No dnf at all (Debian, Ubuntu, Arch, openSUSE).
    NoDnf,
}

/// What root and the user both read, the plan's only inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    pub os: Option<OsRelease>,
    pub ostree: bool,
    pub dnf: bool,
    pub dnf5: bool,
    pub toolkit: Toolkit,
    /// The packages of NVIDIA's toolkit family that are installed (`rpm -q`
    /// of each of [`TOOLKIT_FAMILY`]), in that order.
    pub family: Vec<String>,
    pub keys: Result<ImportedKeys, String>,
    pub repos: Repos,
    pub repo_file: OwnFile,
    pub key_file: OwnFile,
}

impl Inputs {
    /// Read them from the machine `f` describes, with `providers` as the
    /// provider table ([`PROVIDERS`] in production), through root's
    /// environment ([`Facts::root_env`], D87): the user's side and the
    /// helper read the same facts, so a plan the user confirmed is one of
    /// root's unless the machine changed or root may read what the user may
    /// not.
    pub fn read(f: &dyn Facts, providers: &[Provider]) -> Self {
        match f.root_env() {
            Some(root) => Self::read_as_is(&*root, providers),
            None => Self::read_as_is(f, providers),
        }
    }

    fn read_as_is(f: &dyn Facts, providers: &[Provider]) -> Self {
        let os = os_release(f);
        let toolkit = Toolkit::of(owner_of(f, Path::new(NVIDIA_CTK)), os.as_ref(), providers);
        Self {
            ostree: ostree_booted(f),
            dnf: exists(f, Path::new(DNF)),
            dnf5: exists(f, Path::new(DNF5)),
            toolkit,
            family: TOOLKIT_FAMILY
                .iter()
                .filter(|name| query(f, name).is_ok_and(|p| !p.is_empty()))
                .map(|name| name.to_string())
                .collect(),
            keys: imported_keys(f),
            repos: read_repos(f),
            repo_file: sections::repo_file(f),
            key_file: sections::key_file(f),
            os,
        }
    }

    pub fn system(&self) -> System {
        if self.ostree {
            System::Ostree
        } else if self.dnf5 {
            System::Dnf5
        } else if self.dnf {
            System::OtherDnf
        } else {
            System::NoDnf
        }
    }
}

/// What the user's side asks of the plan beyond the machine's facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Want {
    /// lmgw's spec generation failed with NVIDIA's toolkit: offer S4′.
    pub update_toolkit: bool,
}

/// Why the steps a machine needs cannot be planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// A file at one of lmgw's two paths that lmgw did not write.
    FileInTheWay(String),
    /// rpm could not say who owns `nvidia-ctk`.
    Rpm(String),
    /// `rpmkeys --list` failed.
    Keys(String),
    /// A package or section name that is not one (it would be an option).
    BadName(String),
    /// The section the update goes through does not offer a toolkit package
    /// that is installed and has to move with the others.
    NotOffered { section: String, package: String },
}

impl Blocker {
    /// What the person can do about it, in plain words.
    pub fn what_to_do(&self) -> String {
        match self {
            Self::FileInTheWay(_) => {
                "If you don't need that file any more, remove it, then check again.".into()
            }
            Self::Rpm(_) | Self::Keys(_) => "Another program may be installing software right \
                 now: wait until it is done, then check again."
                .into(),
            Self::BadName(_) => format!(
                "NVIDIA's install guide has the commands to add the piece by hand: \
                 {}",
                embedded::NVIDIA_INSTALL_GUIDE
            ),
            Self::NotOffered { .. } => "Switch on a software source that offers it, or update \
                 the pieces from where they came from, then check again."
                .into(),
        }
    }
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileInTheWay(why) => write!(f, "{why}, and lmgw never overwrites it"),
            Self::Rpm(why) => write!(f, "the package database could not be read ({why})"),
            Self::Keys(why) => write!(f, "rpm's trusted keys could not be read ({why})"),
            Self::BadName(name) => write!(f, "'{name}' is not a name lmgw passes to dnf"),
            Self::NotOffered { section, package } => write!(
                f,
                "the software source {section} does not offer {package}, which is installed and \
                 has to be updated with the rest"
            ),
        }
    }
}

/// The plan for one machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub system: System,
    pub toolkit: Toolkit,
    /// The section its dnf steps are scoped to.
    pub section: Option<Section>,
    /// What runs as root, in order.
    pub steps: Vec<Step>,
    /// Why steps this machine needs could not be planned; when not empty,
    /// `steps` is empty.
    pub blocked: Vec<Blocker>,
}

impl Plan {
    /// The struct the hash covers.
    pub fn hashed(&self) -> HashedPlan {
        HashedPlan::of(&self.steps)
    }

    /// [`HashedPlan::sha256`] of [`Self::hashed`].
    pub fn hash(&self) -> String {
        self.hashed().sha256()
    }

    /// Does this machine need something the plan cannot carry out here (the
    /// toolkit is missing on a system without dnf)?
    pub fn needs_toolkit(&self) -> bool {
        matches!(self.toolkit, Toolkit::Absent)
    }
}

/// The toolkit step this machine needs, before any section is chosen.
enum ToolkitNeed {
    None,
    Install,
    Update,
    Swap(PackageName),
}

/// Plan the steps for `inputs`.
pub fn plan(inputs: &Inputs, want: Want, providers: &[Provider]) -> Plan {
    let system = inputs.system();
    let mut out = Plan {
        system,
        toolkit: inputs.toolkit.clone(),
        section: None,
        steps: Vec::new(),
        blocked: Vec::new(),
    };
    let need = match &inputs.toolkit {
        Toolkit::Absent => ToolkitNeed::Install,
        Toolkit::Nvidia(_) if want.update_toolkit => ToolkitNeed::Update,
        Toolkit::Retired(p) => match PackageName::new(&p.name) {
            Some(name) => ToolkitNeed::Swap(name),
            None => {
                out.blocked.push(Blocker::BadName(p.name.clone()));
                return out;
            }
        },
        Toolkit::Unknown(why) => {
            out.blocked.push(Blocker::Rpm(why.clone()));
            return out;
        }
        _ => ToolkitNeed::None,
    };
    match system {
        System::NoDnf | System::OtherDnf => {
            // No commands of lmgw's for these yet: the instructions point at
            // NVIDIA's guide (§3.7). An older dnf gets nothing to type either:
            // the plan's commands are dnf5's.
            return out;
        }
        System::Ostree => {
            // Install only: rpm-ostree reads the sections as they are, so
            // lmgw's repository and key file come first; it checks
            // signatures through the section's `gpgkey`, so nothing is
            // imported into rpm's keys.
            if !matches!(need, ToolkitNeed::Install) {
                return out;
            }
            // Enabled sections only: switching one on is a dnf5 command,
            // and rpm-ostree does not read dnf5's override files.
            let section =
                sections::choose(&inputs.repos, inputs.os.as_ref(), providers, TOOLKIT_BASE)
                    .filter(|s| s.enabled);
            plan_sources(inputs, &section, false, &mut out);
            if out.blocked.is_empty() {
                let package = section
                    .as_ref()
                    .and_then(|s| PackageName::new(&s.package))
                    .unwrap_or_else(steps::toolkit_base);
                out.steps.push(Step::Install {
                    via: Installer::RpmOstree,
                    package,
                });
                out.section = section;
            } else {
                out.steps.clear();
            }
            return out;
        }
        System::Dnf5 => {}
    }
    let section = sections::choose(&inputs.repos, inputs.os.as_ref(), providers, TOOLKIT_BASE);
    let wants_toolkit = !matches!(need, ToolkitNeed::None);
    // lmgw keeps its own repository file and key current even when no
    // toolkit step is due (§3.2, "When NVIDIA changes its key").
    let keeps_own = !matches!(inputs.repo_file, OwnFile::Absent | OwnFile::NotOwn(_));
    if !wants_toolkit && !keeps_own {
        return out;
    }
    if wants_toolkit {
        plan_sources(inputs, &section, true, &mut out);
    } else {
        maintain_own(inputs, &mut out);
    }
    if !out.blocked.is_empty() {
        out.steps.clear();
        return out;
    }
    if !wants_toolkit {
        return out;
    }
    // The section the toolkit comes from: the one chosen, or lmgw's own,
    // which S1 has just added.
    let section = section.unwrap_or_else(sections::own_section);
    let nvidia_key = section.origin != Origin::Provider;
    let (Some(repo), Some(package)) =
        (RepoId::new(&section.id), PackageName::new(&section.package))
    else {
        out.blocked.push(Blocker::BadName(format!(
            "{} / {}",
            section.id, section.package
        )));
        out.steps.clear();
        return out;
    };
    let mut scope = Scope {
        repo,
        nvidia_key,
        include: Vec::new(),
    };
    match need {
        ToolkitNeed::None => {}
        ToolkitNeed::Install => out.steps.push(Step::Install {
            via: Installer::Dnf(scope),
            package,
        }),
        ToolkitNeed::Update => {
            // The owner package first, so the list is never empty; then the
            // rest of NVIDIA's installed toolkit packages, which pin each
            // other's versions and so move together.
            let first = match inputs
                .toolkit
                .package()
                .and_then(|p| PackageName::new(&p.name))
            {
                Some(p) => p,
                None => steps::toolkit_base(),
            };
            let rest: Vec<PackageName> = inputs
                .family
                .iter()
                .filter(|name| name.as_str() != first.as_str())
                .map(|name| PackageName::new(name).expect("a constant package name"))
                .collect();
            let packages = NonEmpty::new(first, rest);
            // A section that filters lets each of them through: lmgw's own
            // lets only `-base` through in its file, so the others are
            // appended for this command; a person's section that filters
            // them out is theirs to change.
            for p in packages
                .iter()
                .filter(|p| !section.lets_through(p.as_str()))
            {
                if section.origin == Origin::Lmgw {
                    scope.include.push(p.clone());
                } else {
                    out.blocked.push(Blocker::NotOffered {
                        section: section.id.clone(),
                        package: p.as_str().to_string(),
                    });
                }
            }
            if !out.blocked.is_empty() {
                out.steps.clear();
                return out;
            }
            out.steps.push(Step::Update { scope, packages });
        }
        ToolkitNeed::Swap(from) => out.steps.push(Step::Swap {
            scope,
            from,
            to: package,
        }),
    }
    out.section = Some(section);
    out
}

/// The toolkit from another enabled repository that the user's package
/// metadata says offers it (§3.2, "Another repository"): its install,
/// scoped to that repository alone with the system's own signature checks,
/// and nothing of NVIDIA's added next to it. Only ever shown as
/// instructions: root does not read the user's metadata, so its plan never
/// has this step and the helper would refuse this hash.
pub fn plan_from_other(inputs: &Inputs, repo: &str, package: &str) -> Plan {
    let system = inputs.system();
    let mut out = Plan {
        system,
        toolkit: inputs.toolkit.clone(),
        section: None,
        steps: Vec::new(),
        blocked: Vec::new(),
    };
    let (Some(id), Some(name)) = (RepoId::new(repo), PackageName::new(package)) else {
        out.blocked
            .push(Blocker::BadName(format!("{repo} / {package}")));
        return out;
    };
    let via = match system {
        System::Dnf5 => Installer::Dnf(Scope {
            repo: id,
            nvidia_key: false,
            include: Vec::new(),
        }),
        // rpm-ostree reads every enabled section, that one included.
        System::Ostree => Installer::RpmOstree,
        System::NoDnf | System::OtherDnf => return out,
    };
    out.steps.push(Step::Install { via, package: name });
    out.section = Some(Section {
        id: repo.to_string(),
        origin: Origin::Person,
        enabled: true,
        package: package.to_string(),
        includepkgs: Vec::new(),
    });
    out
}

/// S1 or S1′, then S2, for a toolkit step from `section` (`None`: lmgw adds
/// its own). A provider-table section needs neither. `import` is false on an
/// image-based system, whose rpm keys are part of the image.
fn plan_sources(inputs: &Inputs, section: &Option<Section>, import: bool, out: &mut Plan) {
    match section {
        Some(s) if s.origin == Origin::Provider => return,
        Some(s) => {
            if s.origin == Origin::Lmgw && inputs.repo_file == OwnFile::OlderOwn {
                out.steps.push(Step::add_repo(true));
            }
            if !s.enabled {
                match RepoId::new(&s.id) {
                    Some(repo) => out.steps.push(Step::EnableRepo { repo }),
                    None => out.blocked.push(Blocker::BadName(s.id.clone())),
                }
            }
        }
        None => match &inputs.repo_file {
            OwnFile::Absent => out.steps.push(Step::add_repo(false)),
            OwnFile::OlderOwn => out.steps.push(Step::add_repo(true)),
            // Current, yet no section of it was read: an override dropped
            // it or its baseurl. Not lmgw's to rewrite again.
            OwnFile::Current => out.blocked.push(Blocker::FileInTheWay(format!(
                "{} is lmgw's, but dnf does not read it as NVIDIA's source",
                embedded::REPO_FILE
            ))),
            OwnFile::NotOwn(why) => out.blocked.push(Blocker::FileInTheWay(why.clone())),
        },
    }
    add_key(inputs, import, out);
}

/// lmgw's own repository file and key, kept current with no toolkit step due.
fn maintain_own(inputs: &Inputs, out: &mut Plan) {
    if inputs.repo_file == OwnFile::OlderOwn {
        out.steps.push(Step::add_repo(true));
    }
    add_key(inputs, true, out);
}

/// S2, when lmgw's key file is not there with this lmgw's key, or (`import`)
/// rpm does not trust the key yet.
fn add_key(inputs: &Inputs, import: bool, out: &mut Plan) {
    let write = match &inputs.key_file {
        OwnFile::Current => None,
        OwnFile::Absent => Some(false),
        OwnFile::OlderOwn => Some(true),
        OwnFile::NotOwn(why) => {
            out.blocked.push(Blocker::FileInTheWay(why.clone()));
            return;
        }
    };
    let imported = match &inputs.keys {
        Ok(keys) => keys.contains(NVIDIA_KEY_FINGERPRINT),
        Err(e) if import => {
            out.blocked.push(Blocker::Keys(e.clone()));
            return;
        }
        Err(_) => true,
    };
    let import = import && !imported;
    if write.is_some() || import {
        out.steps.push(Step::add_key(write, import));
    }
}

/// Every hash root accepts for the machine `inputs` describes: the plan for
/// each value of the one flag only the user's side knows
/// ([`Want::update_toolkit`]), without repeats.
pub fn accepted_hashes(inputs: &Inputs, providers: &[Provider]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for update_toolkit in [false, true] {
        let h = plan(inputs, Want { update_toolkit }, providers).hash();
        if !out.contains(&h) {
            out.push(h);
        }
    }
    out
}
