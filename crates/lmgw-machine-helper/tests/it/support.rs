//! A stated Fedora 44 for the helper, a fake host that records every effect,
//! and one call that runs the helper on them.

use std::ffi::OsString;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use lmgw_machine::fake::FakeFacts;
use lmgw_machine::helper::{Event, Exit, PlanVariant, Stream, UNIT_CGROUP};
use lmgw_machine::plan::embedded::NVIDIA_KEY_FINGERPRINT;
use lmgw_machine::plan::steps::FileWrite;
use lmgw_machine::plan::Plan;
use lmgw_machine::CmdOut;
use lmgw_machine_helper::{root_plans, Caller, Ended, Host};

/// NVIDIA's stable section as NVIDIA's docs have people add it.
pub const NVIDIA_REPO: &str = "\
[nvidia-container-toolkit]
name=nvidia-container-toolkit
baseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch
repo_gpgcheck=1
gpgcheck=0
enabled=1
gpgkey=https://nvidia.github.io/libnvidia-container/gpgkey
";

/// rpm's query line for `name` 1.20.1, its header signed by NVIDIA.
fn nvidia_line(name: &str) -> String {
    format!(
        "{name}\t(none)\t1.20.1\t1\tx86_64\tRSA/SHA512, Fri Sep 18 23:09:15 2026, Key ID \
         ddcae044f796ecb0\t(none)\n"
    )
}

/// What the stated rpm and systemd answer; a test changes it through the
/// shared handle.
#[derive(Default)]
pub struct Answers {
    /// NVIDIA's `-base` owns `/usr/bin/nvidia-ctk`.
    pub nvidia_toolkit: bool,
    /// Fedora's retired Go package owns it.
    pub retired_toolkit: bool,
    /// rpm trusts NVIDIA's key.
    pub trusted: bool,
    /// `rpmkeys --list` fails.
    pub keys_fail: bool,
    /// `systemctl show`'s ActiveState for the unit (`None`: systemctl fails).
    pub unit_state: Option<String>,
}

pub struct Machine {
    pub f: FakeFacts,
    pub answers: Arc<Mutex<Answers>>,
}

impl Machine {
    /// Fedora 44 with dnf5 and rpm, no toolkit, no NVIDIA section, NVIDIA's
    /// key not trusted, the unit not running.
    pub fn fedora() -> Self {
        let f = FakeFacts::new();
        f.set_uid(0);
        f.file(
            "/etc/os-release",
            "NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=44\n",
        );
        f.file(
            "/etc/yum.repos.d/fedora.repo",
            "[fedora]\nname=Fedora\nenabled=1\n",
        );
        f.file_mode("/usr/bin/dnf5", b"\x7fELF", 0o755);
        f.link("/usr/bin/dnf", "dnf5");
        let answers = Arc::new(Mutex::new(Answers {
            unit_state: Some("inactive".into()),
            ..Answers::default()
        }));
        let a = answers.clone();
        f.program("/usr/bin/rpm", move |args| {
            let a = a.lock().unwrap();
            let target = args.last().map(String::as_str).unwrap_or("");
            Ok(match (args[0].as_str(), target) {
                ("-qf", "/usr/bin/nvidia-ctk") if a.nvidia_toolkit => {
                    CmdOut::success(nvidia_line("nvidia-container-toolkit-base"))
                }
                ("-qf", "/usr/bin/nvidia-ctk") if a.retired_toolkit => CmdOut::success(
                    "golang-github-nvidia-container-toolkit\t(none)\t1.17.4\t3.fc43\tx86_64\t\
                     RSA/SHA256, Tue Jul 22 10:00:00 2025, Key ID c8ac4916105ef944\t(none)\n",
                ),
                ("-q", "nvidia-container-toolkit-base") if a.nvidia_toolkit => {
                    CmdOut::success(nvidia_line("nvidia-container-toolkit-base"))
                }
                ("-q", name) => CmdOut::failure(1, format!("package {name} is not installed\n")),
                _ => CmdOut::failure(1, "error: unexpected query\n"),
            })
        });
        let a = answers.clone();
        f.program("/usr/bin/rpmkeys", move |_| {
            let a = a.lock().unwrap();
            if a.keys_fail {
                return Ok(CmdOut::failure(1, "error: cannot open Packages database\n"));
            }
            Ok(CmdOut::success(if a.trusted {
                format!("{NVIDIA_KEY_FINGERPRINT} NVIDIA CORPORATION public key\n")
            } else {
                String::new()
            }))
        });
        let a = answers.clone();
        f.program("/usr/bin/systemctl", move |_| {
            Ok(match &a.lock().unwrap().unit_state {
                Some(state) => CmdOut::success(format!("{state}\n")),
                None => CmdOut::failure(1, "Failed to connect to bus: No such file or directory\n"),
            })
        });
        Self { f, answers }
    }

    /// NVIDIA's own `-base` 1.20.1 installed from the person's NVIDIA
    /// section, its key trusted: nothing to do until lmgw's spec generation
    /// fails, then lmgw's key file and S4′.
    pub fn with_nvidia_toolkit(self) -> Self {
        self.f
            .file_mode("/usr/bin/nvidia-ctk", b"\x7fELF", 0o755)
            .file(
                "/etc/yum.repos.d/nvidia-container-toolkit.repo",
                NVIDIA_REPO,
            );
        {
            let mut a = self.answers.lock().unwrap();
            a.nvidia_toolkit = true;
            a.trusted = true;
        }
        self
    }

    /// Fedora's retired Go package owns `nvidia-ctk` (S4″ swaps it).
    pub fn with_retired_toolkit(self) -> Self {
        self.f.file_mode("/usr/bin/nvidia-ctk", b"\x7fELF", 0o755);
        self.answers.lock().unwrap().retired_toolkit = true;
        self
    }

    /// The person's NVIDIA section, switched off in its file (S1′ switches
    /// it on).
    pub fn with_switched_off_section(self) -> Self {
        self.f.file(
            "/etc/yum.repos.d/nvidia-container-toolkit.repo",
            NVIDIA_REPO.replace("enabled=1", "enabled=0"),
        );
        self
    }

    /// An image-based system (`/run/ostree-booted`).
    pub fn ostree(self) -> Self {
        self.f.file("/run/ostree-booted", b"");
        self
    }

    /// Root's plans, as the helper computes them.
    pub fn plans(&self) -> Vec<(PlanVariant, Plan)> {
        root_plans(&self.f)
    }

    /// The hash of root's plan without (`false`) or with the toolkit update.
    pub fn hash(&self, update_toolkit: bool) -> String {
        self.plans()
            .into_iter()
            .find(|(v, _)| v.update_toolkit == update_toolkit)
            .map(|(v, _)| v.sha256)
            .unwrap()
    }

    pub fn plan(&self, update_toolkit: bool) -> Plan {
        self.plans()
            .into_iter()
            .find(|(v, _)| v.update_toolkit == update_toolkit)
            .map(|(_, p)| p)
            .unwrap()
    }

    pub fn set_unit_state(&self, state: Option<&str>) {
        self.answers.lock().unwrap().unit_state = state.map(str::to_string);
    }
}

/// How the fake host answers a program: its output lines, and how it ended.
pub type Answer =
    Arc<dyn Fn(&[String]) -> (Vec<(Stream, String)>, io::Result<Ended>) + Send + Sync>;

/// A host that records every effect and starts nothing.
pub struct FakeHost {
    pub runs: Mutex<Vec<Vec<String>>>,
    /// (path, replace, content) of every file written.
    pub writes: Mutex<Vec<(String, bool, String)>>,
    /// (priority, message, event line) of every journal entry.
    pub journal: Mutex<Vec<(u8, String, String)>>,
    answer: Mutex<Answer>,
    /// A path whose write fails.
    pub write_fails: Mutex<Option<String>>,
    /// The journal takes nothing.
    pub journal_fails: Mutex<bool>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHost {
    /// Every program exits 0 and prints nothing.
    pub fn new() -> Self {
        Self {
            runs: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            journal: Mutex::new(Vec::new()),
            answer: Mutex::new(Arc::new(|_| (Vec::new(), Ok(Ended::exited(0))))),
            write_fails: Mutex::new(None),
            journal_fails: Mutex::new(false),
        }
    }

    pub fn answer(
        &self,
        f: impl Fn(&[String]) -> (Vec<(Stream, String)>, io::Result<Ended>) + Send + Sync + 'static,
    ) -> &Self {
        *self.answer.lock().unwrap() = Arc::new(f);
        self
    }

    pub fn runs(&self) -> Vec<Vec<String>> {
        self.runs.lock().unwrap().clone()
    }

    pub fn writes(&self) -> Vec<(String, bool, String)> {
        self.writes.lock().unwrap().clone()
    }

    pub fn journal(&self) -> Vec<(u8, String, String)> {
        self.journal.lock().unwrap().clone()
    }

    /// Nothing ran, nothing was written.
    pub fn untouched(&self) -> bool {
        self.runs().is_empty() && self.writes().is_empty()
    }
}

impl Host for FakeHost {
    fn run(&self, argv: &[String], line: &mut dyn FnMut(Stream, &str)) -> io::Result<Ended> {
        self.runs.lock().unwrap().push(argv.to_vec());
        let answer = self.answer.lock().unwrap().clone();
        let (lines, ended) = answer(argv);
        for (stream, text) in lines {
            line(stream, &text);
        }
        ended
    }

    fn write_file(&self, w: &FileWrite) -> io::Result<()> {
        if self.write_fails.lock().unwrap().as_deref() == Some(w.path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Permission denied (os error 13)",
            ));
        }
        self.writes
            .lock()
            .unwrap()
            .push((w.path.to_string(), w.replace, w.content.to_string()));
        Ok(())
    }

    fn journal(&self, priority: u8, message: &str, event: &str) -> io::Result<()> {
        if *self.journal_fails.lock().unwrap() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "No such file or directory (os error 2)",
            ));
        }
        self.journal
            .lock()
            .unwrap()
            .push((priority, message.to_string(), event.to_string()));
        Ok(())
    }
}

/// Root, started by pkexec from the desktop session: in lmgw's own scope.
pub fn through_pkexec() -> Caller {
    Caller {
        ruid: 0,
        euid: 0,
        pkexec_uid: Some("1000".into()),
        cgroup: Ok(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-lmgw.scope\n".into(),
        ),
    }
}

/// Root inside the system task.
pub fn in_the_unit() -> Caller {
    Caller {
        ruid: 0,
        euid: 0,
        pkexec_uid: None,
        cgroup: Ok(format!("0::{UNIT_CGROUP}\n")),
    }
}

/// What a run of the helper printed and how it ended.
pub struct Outcome {
    pub exit: Exit,
    /// Every stdout line, parsed: a line that is not an event fails the
    /// test, since lmgw's pipe carries events only.
    pub events: Vec<Event>,
    pub stderr: String,
}

impl Outcome {
    /// The events' names, in order (`step_started`, …).
    pub fn names(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|e| {
                serde_json::from_str::<serde_json::Value>(&e.to_line()).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }
}

/// Run the helper with `args` as `caller` on machine `m` and host `host`.
pub fn helper(args: &[&str], caller: &Caller, m: &Machine, host: &FakeHost) -> Outcome {
    helper_into(args, caller, m, host, &mut Vec::new())
}

/// [`helper`], its stdout written into `out` (a writer that fails, say).
pub fn helper_into(
    args: &[&str],
    caller: &Caller,
    m: &Machine,
    host: &FakeHost,
    out: &mut dyn Write,
) -> Outcome {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    helper_os(&args, caller, m, host, out)
}

pub fn helper_os(
    args: &[OsString],
    caller: &Caller,
    m: &Machine,
    host: &FakeHost,
    out: &mut dyn Write,
) -> Outcome {
    let mut tee = Tee {
        inner: out,
        seen: Vec::new(),
    };
    let mut err = Vec::new();
    let exit = lmgw_machine_helper::main(args, caller, &m.f, host, &mut tee, &mut err);
    let text = String::from_utf8(tee.seen).unwrap();
    let events = text
        .lines()
        .map(|l| Event::parse(l).unwrap_or_else(|| panic!("not an event line: {l}")))
        .collect();
    Outcome {
        exit,
        events,
        stderr: String::from_utf8(err).unwrap(),
    }
}

/// Records what the helper wrote, then hands it to `inner` (whose errors the
/// helper sees).
struct Tee<'a> {
    inner: &'a mut dyn Write,
    seen: Vec<u8>,
}

impl Write for Tee<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.seen.extend_from_slice(buf);
        self.inner.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A stdout whose reader is gone: every write fails with a broken pipe.
pub struct ClosedPipe;

impl Write for ClosedPipe {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

/// `run`'s first line inside the unit.
pub fn runs_begun() -> (Stream, String) {
    (
        Stream::Stdout,
        Event::Begun {
            verb: "run".into(),
            version: lmgw_machine_helper::VERSION.into(),
            schema: lmgw_machine::plan::PLAN_SCHEMA,
            pkexec_uid: None,
        }
        .to_line(),
    )
}

/// What the system task prints for a run whose steps all ran (`exit` 0) or
/// stopped at a failed one (3), up to `run`'s `finished`: what a fake
/// `systemd-run` answers.
pub fn unit_lines(sha256: &str, exit: i32) -> Vec<(Stream, String)> {
    vec![
        runs_begun(),
        (
            Stream::Stdout,
            Event::Started {
                sha256: sha256.into(),
                steps: 3,
            }
            .to_line(),
        ),
        (Stream::Stdout, Event::Finished { exit }.to_line()),
    ]
}

/// What the system task prints when `run` ends with `code` by its own
/// route: `finished` for 0 and 3, `plan_changed` for 2 (the machine changed
/// between `apply` and the unit), `refused` for 4.
pub fn runs_lines_for(sha256: &str, code: i32) -> Vec<(Stream, String)> {
    match code {
        0 | 3 => unit_lines(sha256, code),
        2 => vec![
            runs_begun(),
            (
                Stream::Stdout,
                lmgw_machine_helper::plan_changed(sha256, Vec::new()).to_line(),
            ),
        ],
        4 => vec![(
            Stream::Stdout,
            Event::Refused {
                why: "run runs only inside the system task".into(),
            }
            .to_line(),
        )],
        other => panic!("run has no own route to exit {other}"),
    }
}
