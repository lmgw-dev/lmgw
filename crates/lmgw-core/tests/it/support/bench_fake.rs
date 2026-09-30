//! The benchmark suite's fakes (`bench.rs`, `bench_lease.rs`): a
//! [`FakeLauncher`] whose port is a fake llama-server's and whose podman
//! verbs are recorded and answered from fields, a registry podman whose `ps`
//! lists what a test says ([`PsRunner`]), and a GPU probe that holds one
//! `devices()` call in flight ([`HeldProbe`]).

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use lmgw_core::bench::BenchLauncher;
use lmgw_core::runtime::registry::{CmdOutput, CommandRunner};
use lmgw_core::vram::{GpuMemory, GpuPower, GpuProbe, ProcessMemory};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// The fake launcher
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct FakeLauncher {
    /// The port "the container" answers on: the fake llama-server's.
    pub port: u16,
    pub calls: Mutex<Vec<Vec<String>>>,
    /// Bench containers `ps` lists (the boot sweep's leftovers).
    pub leftovers: Mutex<Vec<String>>,
    /// Their `Created` (unix seconds) where not 0, the epoch.
    pub created: Mutex<std::collections::HashMap<String, i64>>,
    /// `image inspect` says the image is not here.
    pub image_missing: AtomicBool,
    /// `inspect` says the container exited (a load that died).
    pub exited: AtomicBool,
    /// The verb whose call panics — a bug in the run, as the job sees it.
    pub panic_on: Mutex<Option<String>>,
    /// `rm` fails, as podman does when its storage is wedged.
    pub rm_fails: AtomicBool,
    /// The verb whose call never returns — podman stalled on its storage
    /// lock — until the caller drops it.
    pub stall_on: Mutex<Option<String>>,
    /// Notified by `rm`: the fake llama-server's connections break.
    pub rm_kills: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl FakeLauncher {
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    /// The argv of every call whose verb is `verb`.
    pub fn verb(&self, verb: &str) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c.first().map(String::as_str) == Some(verb))
            .collect()
    }

    pub fn removed(&self, name: &str) -> bool {
        self.verb("rm")
            .iter()
            .any(|c| c.last().map(String::as_str) == Some(name))
    }
}

pub fn out(status: i32, stdout: &str, stderr: &str) -> std::io::Result<CmdOutput> {
    Ok(CmdOutput {
        status,
        stdout: stdout.into(),
        stderr: stderr.into(),
    })
}

#[async_trait::async_trait]
impl BenchLauncher for FakeLauncher {
    fn free_port(&self) -> std::io::Result<u16> {
        Ok(self.port)
    }

    async fn podman(&self, argv: &[String]) -> std::io::Result<CmdOutput> {
        self.calls.lock().unwrap().push(argv.to_vec());
        if self.panic_on.lock().unwrap().as_deref() == Some(argv[0].as_str()) {
            panic!("the fake podman panicked on '{}'", argv[0]);
        }
        let stall = self.stall_on.lock().unwrap().as_deref() == Some(argv[0].as_str());
        if stall {
            std::future::pending::<()>().await;
        }
        match argv[0].as_str() {
            "image" if self.image_missing.load(Ordering::SeqCst) => {
                out(125, "", "Error: image not known")
            }
            "image" => out(
                0,
                "sha256:feedbeef {\"dev.lmgw.slug\":\"official-master\",\
                 \"org.opencontainers.image.version\":\"b11226\"}\n",
                "",
            ),
            "ps" => {
                let created = self.created.lock().unwrap().clone();
                let rows: Vec<Value> = self
                    .leftovers
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|n| {
                        let prefix = n.split("-bench-").next().unwrap_or_default();
                        json!({"Names": [n],
                               "Labels": {"lmgw.bench": "99", "lmgw.instance": prefix},
                               "Created": created.get(n).copied().unwrap_or(0)})
                    })
                    .collect();
                out(0, &serde_json::to_string(&rows).unwrap(), "")
            }
            "rm" if self.rm_fails.load(Ordering::SeqCst) => {
                out(125, "", "Error: container state improper")
            }
            "rm" => {
                if let Some(kill) = self.rm_kills.lock().unwrap().clone() {
                    kill.notify_waiters();
                }
                let name = argv.last().cloned().unwrap_or_default();
                self.leftovers.lock().unwrap().retain(|n| *n != name);
                out(0, "", "")
            }
            "logs" => out(
                0,
                "",
                "ggml_cuda: cudaMalloc failed: out of memory\nexiting\n",
            ),
            "inspect" if self.exited.load(Ordering::SeqCst) => out(
                0,
                r#"[{"State":{"Status":"exited","Running":false,"ExitCode":1}}]"#,
                "",
            ),
            "inspect" => out(
                0,
                r#"[{"State":{"Status":"running","Running":true,"ExitCode":0}}]"#,
                "",
            ),
            _ => out(0, "c0ffee\n", ""),
        }
    }
}

/// A registry podman whose `ps` lists one bench container of this instance.
pub struct PsRunner {
    pub ps: String,
    pub calls: Mutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl CommandRunner for PsRunner {
    async fn run(&self, _program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        self.calls.lock().unwrap().push(args.to_vec());
        match args[0].as_str() {
            "ps" => out(0, &self.ps, ""),
            _ => out(0, "", ""),
        }
    }
}

// ---------------------------------------------------------------------------
// A probe that holds one read in flight
// ---------------------------------------------------------------------------

/// The card's own probe, except that the next `devices()` call after
/// [`Self::hold_next`] waits for [`Self::release`] — an admission decision
/// held between its lease check and its claim, inside its ledger read.
pub struct HeldProbe {
    inner: Arc<dyn GpuProbe>,
    st: Mutex<Held>,
    cv: Condvar,
}

#[derive(Default)]
struct Held {
    armed: bool,
    holding: bool,
    /// What the held call answers; `None`: the card's own answer.
    answer: Option<Result<Vec<GpuMemory>, String>>,
}

impl HeldProbe {
    pub fn wrap(inner: Arc<dyn GpuProbe>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            st: Mutex::new(Held::default()),
            cv: Condvar::new(),
        })
    }

    /// Hold the next `devices()` call until [`Self::release`]; it then
    /// answers `answer`, or the card's own answer for `None`.
    pub fn hold_next(&self, answer: Option<Result<Vec<GpuMemory>, String>>) {
        let mut s = self.st.lock().unwrap();
        s.armed = true;
        s.answer = answer;
    }

    /// Wait until a call is being held.
    pub async fn held(&self) {
        for _ in 0..1000 {
            if self.st.lock().unwrap().holding {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no devices() call was held");
    }

    pub fn release(&self) {
        self.st.lock().unwrap().holding = false;
        self.cv.notify_all();
    }
}

impl GpuProbe for HeldProbe {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        let mut s = self.st.lock().unwrap();
        if s.armed {
            s.armed = false;
            s.holding = true;
            let answer = s.answer.take();
            // Bounded, so a test that fails before releasing does not hang
            // the runtime's shutdown on this blocking thread.
            let (s, _) = self
                .cv
                .wait_timeout_while(s, Duration::from_secs(30), |s| s.holding)
                .unwrap();
            drop(s);
            if let Some(a) = answer {
                return a;
            }
        } else {
            drop(s);
        }
        self.inner.devices()
    }

    fn source(&self) -> String {
        self.inner.source()
    }

    fn process_support(&self) -> Result<(), String> {
        self.inner.process_support()
    }

    fn processes(&self, own: &[u32], retired: &[u32]) -> Result<Vec<ProcessMemory>, String> {
        self.inner.processes(own, retired)
    }

    fn power(&self) -> Result<Vec<GpuPower>, String> {
        self.inner.power()
    }

    fn driver_version(&self) -> Option<String> {
        self.inner.driver_version()
    }
}
