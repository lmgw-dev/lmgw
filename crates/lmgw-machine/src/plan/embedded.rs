//! What the plan writes and runs, fixed in lmgw's source (machine-check
//! design §3.2): the repository file, NVIDIA's signing key, the package
//! names and every program's absolute path.
//!
//! The file contents are plan inputs on purpose: they are what root writes,
//! so they are part of the hash ([`super::hash`]). A changed key or a changed
//! repository text is a reviewed code change, never a download.

/// lmgw's own repository file.
pub const REPO_FILE: &str = "/etc/yum.repos.d/lmgw-nvidia-container-toolkit.repo";

/// The section lmgw's repository file defines.
pub const REPO_ID: &str = "lmgw-nvidia-container-toolkit";

/// The first line of every repository file lmgw writes: a file of
/// [`REPO_FILE`]'s name that starts with it is lmgw's own, and lmgw replaces
/// it when its text differs from [`REPO_TEXT`]. A file without it is the
/// person's, and lmgw never overwrites it.
pub const REPO_MARKER: &str = "# Written by lmgw.";

/// NVIDIA's stable section as NVIDIA publishes it
/// (`https://nvidia.github.io/libnvidia-container/stable/rpm/nvidia-container-toolkit.repo`,
/// fetched 2026-11-03), with lmgw's changes: package checks on against the
/// local key, only the one package lmgw installs, no experimental section.
/// No `skip_if_unavailable=False`: that would fail every system update
/// while nvidia.github.io is down. lmgw's own dnf calls set it ([`super::steps::Scope`]).
pub const REPO_TEXT: &str = "\
# Written by lmgw. lmgw replaces this file when its text or NVIDIA's key changes.
[lmgw-nvidia-container-toolkit]
name=NVIDIA Container Toolkit (added by lmgw)
baseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch
includepkgs=nvidia-container-toolkit-base
repo_gpgcheck=1
gpgcheck=1
enabled=1
gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit
";

/// Where lmgw keeps NVIDIA's signing key.
pub const KEY_FILE: &str = "/etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit";

/// [`KEY_FILE`] as dnf's `gpgkey` names it.
pub const KEY_URL: &str = "file:///etc/pki/rpm-gpg/RPM-GPG-KEY-lmgw-nvidia-container-toolkit";

/// NVIDIA's "NVIDIA CORPORATION (Open Source Projects)
/// <cudatools@nvidia.com>" key, byte for byte as
/// `https://nvidia.github.io/libnvidia-container/gpgkey` served it on
/// 2026-11-03 (its packets identical to the copy dnf caches from NVIDIA's
/// repository). It signed `nvidia-container-toolkit-base-1.20.1-1`. A
/// test pins its fingerprint ([`NVIDIA_KEY_FINGERPRINT`]).
pub const NVIDIA_KEY: &str = include_str!("nvidia-container-toolkit.asc");

/// [`NVIDIA_KEY`]'s fingerprint, as `rpmkeys --list` prints it (lowercase,
/// no spaces): `C95B 321B 61E8 8C18 09C4 F759 DDCA E044 F796 ECB0`.
pub const NVIDIA_KEY_FINGERPRINT: &str = "c95b321b61e88c1809c4f759ddcae044f796ecb0";

/// The key ID rpm names in a package header NVIDIA signed: the
/// fingerprint's last 16 digits.
pub const NVIDIA_KEY_ID: &str = "ddcae044f796ecb0";

/// Every key text lmgw has ever written to [`KEY_FILE`], the current one
/// first. A file there holding one of them is lmgw's own and is replaced
/// when it is not the current one; anything else is the person's file and is
/// never overwritten. An armored key has no comment line lmgw could mark it
/// with without changing NVIDIA's bytes, so the text itself is the mark.
pub const OWN_KEYS: &[&str] = &[NVIDIA_KEY];

/// Every section whose `baseurl` starts with this is NVIDIA's (§3.2, "Which
/// sections count").
pub const NVIDIA_BASEURL_PREFIX: &str = "https://nvidia.github.io/libnvidia-container/";

/// NVIDIA's stable channel: the only one lmgw installs from. A section of
/// NVIDIA's experimental channel is NVIDIA's, and is never used.
pub const NVIDIA_STABLE_PREFIX: &str = "https://nvidia.github.io/libnvidia-container/stable/";

/// The package lmgw installs: `nvidia-ctk`, `nvidia-cdi-hook` and NVIDIA's
/// refresh units, requiring only glibc (checked on 1.20.1).
pub const TOOLKIT_BASE: &str = "nvidia-container-toolkit-base";

/// NVIDIA's full toolkit, which pins [`TOOLKIT_BASE`]'s exact version: when
/// it is installed, an update takes both.
pub const TOOLKIT_FULL: &str = "nvidia-container-toolkit";

/// NVIDIA's toolkit packages that move together: [`TOOLKIT_FULL`] requires
/// [`TOOLKIT_BASE`] at its exact version and `libnvidia-container-tools`
/// from its own release, which requires `libnvidia-container1` at its exact
/// version. S4′ updates every one of them that is installed, in this order.
pub const TOOLKIT_FAMILY: [&str; 4] = [
    TOOLKIT_BASE,
    TOOLKIT_FULL,
    "libnvidia-container-tools",
    "libnvidia-container1",
];

/// The packages [`REPO_TEXT`]'s `includepkgs` lets through. An S4′ through
/// lmgw's own section appends the other installed [`TOOLKIT_FAMILY`]
/// members for that one transaction (`--setopt=<id>.includepkgs=`, which
/// dnf5 appends to the file's list), never in the file.
pub const REPO_INCLUDEPKGS: [&str; 1] = [TOOLKIT_BASE];

/// Fedora's retired Go package (bug 2434398), which an upgrade to Fedora 44
/// can leave installed.
pub const RETIRED_TOOLKIT: &str = "golang-github-nvidia-container-toolkit";

/// dnf, by its absolute path: dnf5 on Fedora 41 and later, an older dnf
/// elsewhere. Only whether it is there is read (a dnf that is not dnf5).
pub const DNF: &str = "/usr/bin/dnf";

/// dnf5 itself, which every plan command runs: they are dnf5's
/// (`config-manager setopt`, `--repo`), and the button is offered only where
/// this file is there (D48), so what is checked is what runs.
pub const DNF5: &str = "/usr/bin/dnf5";

/// rpm-ostree, on image-based systems.
pub const RPM_OSTREE: &str = "/usr/bin/rpm-ostree";

/// NVIDIA's install guide, for systems the plan has no commands for yet.
pub const NVIDIA_INSTALL_GUIDE: &str =
    "https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html";

/// The root helper (§3.1, built in WP6): what the button starts.
pub const HELPER: &str = "/usr/libexec/lmgw/lmgw-machine-helper";

/// Its polkit action (§3.3).
pub const POLICY: &str = "/usr/share/polkit-1/actions/io.github.lmgw-dev.lmgw.machine-setup.policy";

/// pkexec, by its absolute path, never looked up in `PATH` (§3.4).
pub const PKEXEC: &str = "/usr/bin/pkexec";
