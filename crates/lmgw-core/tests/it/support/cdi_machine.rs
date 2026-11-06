//! A stated NVIDIA machine for lmgw's own CDI spec (machine-check design §6,
//! "lmgw's own spec"): the driver at one version, every path the
//! spec names, `$XDG_RUNTIME_DIR` in a temp dir, an `nvidia-ctk` whose
//! `cdi generate` writes a spec where it is told and counts each time, and a
//! podman whose `--help` lists `--cdi-spec-dir` (or, for one test, not).
//!
//! Nothing of the computer the suite runs on is read or run: the machine is a
//! `FakeFacts`, and the only real files are the spec and stamp lmgw writes
//! into the temp dir.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lmgw_core::erased::Erased;
use lmgw_core::runtime::cdi::OwnSpec;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner};
use lmgw_machine::cdi::own::NVIDIA_CTK;
use lmgw_machine::cdi::spec::{Format, Spec};
use lmgw_machine::fake::FakeFacts;
use lmgw_machine::CmdOut;

/// A spec as `nvidia-ctk cdi generate --format=json` writes it
/// (lmgw-machine's fixture: a rootless spec as JSON).
pub const SPEC: &str = include_str!("../../../../lmgw-machine/tests/fixtures/nvidia.json");

/// The podman lmgw asks whether it takes `--cdi-spec-dir`: its `--help`
/// says so (Podman 5.8.9's line) unless `old`; every `--help` is counted.
#[derive(Default)]
pub struct HelpPodman {
    pub old: bool,
    pub asked: AtomicUsize,
}

impl CommandRunner for HelpPodman {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> Erased<'a, std::io::Result<CmdOutput>> {
        Box::pin(async move {
            assert_eq!(program, "podman");
            assert_eq!(args, ["--help"], "the spec asks podman nothing else");
            self.asked.fetch_add(1, Ordering::SeqCst);
            let mut stdout = String::from(
                "Manage pods, containers and images\n\nUsage:\n  podman [options] [command]\n\nOptions:\n",
            );
            if !self.old {
                stdout.push_str(
                    "      --cdi-spec-dir stringArray    Set the CDI spec directory path (may be \
                     set multiple times) (default [/etc/cdi,/var/run/cdi])\n",
                );
            }
            stdout.push_str("      --cgroup-manager string       Cgroup manager to use\n");
            Ok(CmdOutput {
                status: 0,
                stdout,
                stderr: String::new(),
            })
        })
    }
}

#[derive(Default)]
struct Generator {
    /// What the next `cdi generate` writes; the fixture until a test says.
    spec: Mutex<Option<String>>,
    /// When set, `cdi generate` answers this instead of writing.
    fail: Mutex<Option<CmdOut>>,
    /// How long a generation takes, so two at once really overlap.
    delay: Mutex<Duration>,
    /// When set, `cdi generate` hangs this long (an NVML that does not
    /// answer) and then writes nothing.
    hang: Mutex<Option<Duration>>,
    /// What happens on the machine while `cdi generate` runs.
    #[allow(clippy::type_complexity)]
    during: Mutex<Option<Arc<dyn Fn(&FakeFacts) + Send + Sync>>>,
    count: AtomicUsize,
    /// A toolkit before 1.17: no `--nvidia-cdi-hook-path` in its help.
    old_toolkit: std::sync::atomic::AtomicBool,
}

pub struct CdiMachine {
    pub facts: Arc<FakeFacts>,
    /// `$XDG_RUNTIME_DIR` as the machine reports it.
    pub runtime_dir: tempfile::TempDir,
    generator: Arc<Generator>,
}

impl CdiMachine {
    /// Driver 600.20.03, everything the fixture names on disk.
    pub fn new() -> Self {
        Self::at("600.20.03")
    }

    /// Driver `version` (module and libraries), everything the fixture names
    /// on disk.
    pub fn at(version: &str) -> Self {
        let facts = Arc::new(FakeFacts::nvidia(version));
        add_paths_of(&facts, SPEC);
        let runtime_dir = tempfile::Builder::new()
            .prefix("lmgw-cdi-runtime-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        facts.set_env("XDG_RUNTIME_DIR", runtime_dir.path().display().to_string());
        let m = Self {
            facts,
            runtime_dir,
            generator: Arc::new(Generator::default()),
        };
        m.install_toolkit();
        m
    }

    /// `nvidia-ctk` installed (again): `--version` says 1.20.1, and
    /// `cdi generate` writes as the generator is told to.
    pub fn install_toolkit(&self) {
        let g = self.generator.clone();
        let machine = Arc::downgrade(&self.facts);
        self.facts.program(NVIDIA_CTK, move |args| {
            if args == ["--version"] {
                return Ok(CmdOut::success(
                    "NVIDIA Container Toolkit CLI version 1.20.1\ncommit: 0123456789abcdef\n",
                ));
            }
            if args == ["cdi", "generate", "--help"] {
                let mut text = String::from("USAGE:\n   nvidia-ctk cdi generate [options]\n");
                if !g.old_toolkit.load(Ordering::SeqCst) {
                    text.push_str("   --nvidia-cdi-hook-path value   Specify the path to use for the nvidia-cdi-hook in the generated CDI specification.\n");
                }
                return Ok(CmdOut::success(&text));
            }
            if args.first().map(String::as_str) != Some("cdi")
                || args.get(1).map(String::as_str) != Some("generate")
            {
                return Ok(CmdOut::failure(
                    1,
                    "the fake nvidia-ctk knows only cdi generate",
                ));
            }
            if let Some(out) = g.fail.lock().unwrap().clone() {
                return Ok(out);
            }
            let hang = *g.hang.lock().unwrap();
            if let Some(hang) = hang {
                std::thread::sleep(hang);
                return Ok(CmdOut::failure(1, "the fake nvidia-ctk hung"));
            }
            let delay = *g.delay.lock().unwrap();
            std::thread::sleep(delay);
            let during = g.during.lock().unwrap().clone();
            if let (Some(during), Some(f)) = (during, machine.upgrade()) {
                during(&f);
            }
            let output = args
                .iter()
                .find_map(|a| a.strip_prefix("--output="))
                .expect("lmgw names an --output");
            let text = g
                .spec
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| SPEC.into());
            std::fs::write(output, text)?;
            g.count.fetch_add(1, Ordering::SeqCst);
            Ok(CmdOut {
                status: Some(0),
                stdout: String::new(),
                stderr: "time=\"…\" level=info msg=\"Generated CDI spec with version 0.7.0\"\n"
                    .into(),
            })
        });
    }

    /// `during` happens on the machine while each `cdi generate` runs (a
    /// package transaction landing mid-write).
    pub fn while_generating(&self, during: impl Fn(&FakeFacts) + Send + Sync + 'static) {
        *self.generator.during.lock().unwrap() = Some(Arc::new(during));
    }

    /// lmgw's own spec on this machine, for a gateway on `data_dir`, with a
    /// Podman that takes `--cdi-spec-dir`.
    pub fn own_spec(&self, data_dir: &Path) -> Arc<OwnSpec> {
        self.own_spec_with(data_dir, Arc::new(HelpPodman::default()))
    }

    /// [`Self::own_spec`] with the podman `podman` answers `--help` for.
    pub fn own_spec_with(&self, data_dir: &Path, podman: Arc<HelpPodman>) -> Arc<OwnSpec> {
        Arc::new(OwnSpec::managed(
            self.facts.clone(),
            data_dir.to_path_buf(),
            podman,
        ))
    }

    /// How many specs `cdi generate` wrote.
    pub fn generations(&self) -> usize {
        self.generator.count.load(Ordering::SeqCst)
    }

    /// `cdi generate` answers `out` from now on (`None`: writes again).
    pub fn fail_with(&self, out: Option<CmdOut>) {
        *self.generator.fail.lock().unwrap() = out;
    }

    /// What `cdi generate` writes from now on.
    pub fn generates(&self, spec: &str) {
        *self.generator.spec.lock().unwrap() = Some(spec.to_string());
    }

    /// How long each `cdi generate` takes from now on; a generation that
    /// outlives its limit writes nothing.
    pub fn slow(&self, delay: Duration) {
        *self.generator.delay.lock().unwrap() = delay;
    }

    /// `cdi generate` hangs `for_` (and then writes nothing) from now on.
    pub fn hang(&self, for_: Duration) {
        *self.generator.hang.lock().unwrap() = Some(for_);
    }

    /// This toolkit has no `--nvidia-cdi-hook-path` flag (before 1.17).
    pub fn old_toolkit(&self) {
        self.generator.old_toolkit.store(true, Ordering::SeqCst);
    }

    /// Every `cdi generate` so far (not its `--help`), its arguments.
    pub fn generate_calls(&self) -> Vec<Vec<String>> {
        self.facts
            .calls_of(NVIDIA_CTK)
            .into_iter()
            .filter(|a| {
                a.first().map(String::as_str) == Some("cdi") && !a.contains(&"--help".to_string())
            })
            .collect()
    }

    /// The spec dir instance `prefix` gets:
    /// `$XDG_RUNTIME_DIR/lmgw-machine/<slug of prefix>/cdi`.
    pub fn spec_dir(&self, prefix: &str) -> PathBuf {
        self.runtime_dir
            .path()
            .join("lmgw-machine")
            .join(lmgw_core::runtime::slug(prefix))
            .join("cdi")
    }
}

/// Every host path `spec` names, present on `facts`.
pub fn add_paths_of(facts: &FakeFacts, spec: &str) {
    let spec = Spec::parse(spec.as_bytes(), Format::Json).expect("the spec parses");
    for p in spec.named_paths() {
        facts.file(&p, b"");
    }
}
