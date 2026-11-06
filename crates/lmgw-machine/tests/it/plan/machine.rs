//! A stated Fedora 44 for the plan: os-release, dnf5, an rpm whose answers
//! a test sets (who owns `nvidia-ctk`, whether NVIDIA's full toolkit is
//! installed, which keys rpm trusts), and repository files.

use std::sync::{Arc, Mutex};

use lmgw_machine::fake::FakeFacts;
use lmgw_machine::plan::embedded::{NVIDIA_KEY_FINGERPRINT, REPO_FILE};
use lmgw_machine::plan::{plan, Inputs, Plan, Provider, Want, PROVIDERS};
use lmgw_machine::CmdOut;

/// NVIDIA's stable section as NVIDIA's docs have people add it (this
/// machine's file).
pub const NVIDIA_REPO: &str = "\
[nvidia-container-toolkit]
name=nvidia-container-toolkit
baseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch
repo_gpgcheck=1
gpgcheck=0
enabled=1
gpgkey=https://nvidia.github.io/libnvidia-container/gpgkey

[nvidia-container-toolkit-experimental]
name=nvidia-container-toolkit-experimental
baseurl=https://nvidia.github.io/libnvidia-container/experimental/rpm/$basearch
repo_gpgcheck=1
gpgcheck=0
enabled=0
gpgkey=https://nvidia.github.io/libnvidia-container/gpgkey
";

pub const NVIDIA_REPO_PATH: &str = "/etc/yum.repos.d/nvidia-container-toolkit.repo";

/// The rpm answers a test sets.
#[derive(Default)]
pub struct Rpm {
    /// `rpm -qf /usr/bin/nvidia-ctk`'s line ([`owner_line`]); `None`: not
    /// owned (when the file is there).
    pub owner: Option<String>,
    pub full_toolkit: bool,
    /// `libnvidia-container-tools` and `libnvidia-container1` installed.
    pub libraries: bool,
    /// Fingerprints `rpmkeys --list` prints.
    pub keys: Vec<String>,
    /// `rpmkeys --list` fails.
    pub keys_fail: bool,
}

/// rpm's query line for `name` at `version`, its header signed by `key`.
pub fn owner_line(name: &str, version: &str, key: Option<&str>) -> String {
    let sig = match key {
        Some(k) => format!("RSA/SHA512, Fri Sep 18 23:09:15 2026, Key ID {k}"),
        None => "(none)".into(),
    };
    format!("{name}\t(none)\t{version}\t1\tx86_64\t{sig}\t(none)\n")
}

pub struct Machine {
    pub f: FakeFacts,
    pub rpm: Arc<Mutex<Rpm>>,
}

impl Machine {
    /// Fedora 44 with dnf5 and rpm, no toolkit, no NVIDIA section, NVIDIA's
    /// key not trusted.
    pub fn fedora() -> Self {
        let f = FakeFacts::new();
        f.file(
            "/etc/os-release",
            "NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=44\nPRETTY_NAME=\"Fedora Linux 44 \
             (KDE Plasma Desktop Edition)\"\n",
        );
        f.file(
            "/etc/yum.repos.d/fedora.repo",
            "[fedora]\nname=Fedora\nenabled=1\n",
        );
        f.file_mode("/usr/bin/dnf5", b"\x7fELF", 0o755);
        f.link("/usr/bin/dnf", "dnf5");
        let rpm = Arc::new(Mutex::new(Rpm::default()));
        let r = rpm.clone();
        f.program("/usr/bin/rpm", move |args| {
            let r = r.lock().unwrap();
            let target = args.last().map(String::as_str).unwrap_or("");
            Ok(match (args[0].as_str(), target) {
                ("-qf", "/usr/bin/nvidia-ctk") => match &r.owner {
                    Some(line) => CmdOut::success(line.clone()),
                    None => {
                        CmdOut::failure(1, "file /usr/bin/nvidia-ctk is not owned by any package\n")
                    }
                },
                ("-q", "nvidia-container-toolkit") if r.full_toolkit => {
                    CmdOut::success(owner_line(
                        "nvidia-container-toolkit",
                        "1.20.1",
                        Some("ddcae044f796ecb0"),
                    ))
                }
                ("-q", name @ ("libnvidia-container-tools" | "libnvidia-container1"))
                    if r.libraries =>
                {
                    CmdOut::success(owner_line(name, "1.20.1", Some("ddcae044f796ecb0")))
                }
                ("-q", name) => CmdOut::failure(1, format!("package {name} is not installed\n")),
                _ => CmdOut::failure(1, "error: unexpected query\n"),
            })
        });
        let r = rpm.clone();
        f.program("/usr/bin/rpmkeys", move |_| {
            let r = r.lock().unwrap();
            if r.keys_fail {
                return Ok(CmdOut::failure(1, "error: cannot open Packages database\n"));
            }
            Ok(CmdOut::success(
                r.keys
                    .iter()
                    .map(|k| format!("{k} Someone <someone@example.org> public key\n"))
                    .collect::<String>(),
            ))
        });
        Self { f, rpm }
    }

    /// `nvidia-ctk` there, owned as `line` says (`None`: by no package).
    pub fn toolkit(&self, line: Option<String>) -> &Self {
        self.f.file_mode("/usr/bin/nvidia-ctk", b"\x7fELF", 0o755);
        self.rpm.lock().unwrap().owner = line;
        self
    }

    /// NVIDIA's own `-base` 1.20.1, signed by NVIDIA.
    pub fn nvidia_toolkit(&self) -> &Self {
        self.toolkit(Some(owner_line(
            "nvidia-container-toolkit-base",
            "1.20.1",
            Some("ddcae044f796ecb0"),
        )))
    }

    /// rpm trusts NVIDIA's key.
    pub fn trust_nvidia(&self) -> &Self {
        self.rpm.lock().unwrap().keys = vec![NVIDIA_KEY_FINGERPRINT.to_string()];
        self
    }

    /// The person's NVIDIA section file.
    pub fn nvidia_section(&self) -> &Self {
        self.f.file(NVIDIA_REPO_PATH, NVIDIA_REPO);
        self
    }

    /// lmgw's own repository file with `text`.
    pub fn own_repo(&self, text: &str) -> &Self {
        self.f.file(REPO_FILE, text);
        self
    }

    pub fn inputs(&self) -> Inputs {
        Inputs::read(&self.f, PROVIDERS)
    }

    pub fn plan(&self) -> Plan {
        self.plan_with(Want::default(), PROVIDERS)
    }

    pub fn plan_with(&self, want: Want, providers: &[Provider]) -> Plan {
        plan(&Inputs::read(&self.f, providers), want, providers)
    }
}

/// The kinds of `plan`'s steps, as the API names them.
pub fn kinds(plan: &Plan) -> Vec<&'static str> {
    plan.steps.iter().map(|s| s.kind().as_str()).collect()
}
