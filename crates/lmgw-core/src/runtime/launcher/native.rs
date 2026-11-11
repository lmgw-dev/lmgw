//! The native launcher: each model's engine as a child process of lmgw
//! (Windows design §4.1), the launcher on Windows.
//!
//! **Where the engine comes from.** [`engines`] asks the pinned engines
//! ([`crate::runtime::engine_assets`], §4.2): on Windows the build this
//! machine runs, chosen per engine from the recorded CUDA coverage and
//! installed (downloaded, verified, unpacked) on its first start; on a
//! platform without pins the one hand-installed build under
//! `<engines>/<engine>/<build>/`. A missing engine is the start's error,
//! naming the path it looked for. A CUDA build whose start ends in "no
//! kernel image is available" is marked, and the registry's start goes on
//! with that engine's next build, its Vulkan one ([`exit`]).
//!
//! **What it runs.** The engine's own argv in the host's view
//! ([`crate::runtime::argv::engine_argv`] with
//! [`EngineView::host`](crate::runtime::argv::EngineView::host): the host's
//! models directory, `127.0.0.1:<port>`), from the engine's install
//! directory, with the card selected through the environment ([`gpu`]).
//! audio.cpp's `server.json` is rendered in that view too, into a `native/`
//! directory beside the container's, which stays what the stale check reads.
//!
//! **Lifetime.** Nothing outlives lmgw, so nothing is listed or adopted at
//! boot. On Windows every child is in one Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, whose handle this launcher owns: when
//! it is dropped, or lmgw exits or crashes, Windows kills the engines. A
//! child is created suspended and runs only once it is in the job; the run
//! args' `--memory` and `--cpus` are a nested job of its own ([`limits`]). On
//! Linux every child gets `PR_SET_PDEATHSIG(SIGKILL)` and a process group of
//! its own. The death signal follows the *thread* that forked the child, not
//! the process, so every child is spawned from one thread this launcher owns
//! ([`spawner`]) and that thread ends only with the launcher: a dropped
//! launcher, or a dead lmgw, kills its engines.
//!
//! **Stop.** Linux: `SIGTERM` to the child's process group, `SIGKILL` after
//! the registry's stop grace (`STOP_GRACE_SECONDS`), then the wait for the
//! exit within the unload timeout — what `podman stop -t 10` and `podman
//! wait` do for a container. Windows: `TerminateProcess` at once. A console
//! break (`CTRL_BREAK_EVENT` to a new process group) is no graceful route
//! there: `GenerateConsoleCtrlEvent` reaches only processes on the caller's
//! console, which a GUI lmgw does not have, and llama-server's console
//! handler acts on `CTRL_C_EVENT` alone, which a process group cannot be
//! sent. The engines keep nothing that a stop would have to save: weights
//! are read-only maps and the driver frees the card's memory at the exit.
//!
//! **Logs.** Each child's stdout and stderr are read into an in-memory ring
//! of the last [`LOG_RING_LINES`] lines ([`ring`]), which `logs_tail` and a
//! failed start's excerpt read. It is kept until the next start under the
//! name or a remove, and the first line it drops is said in lmgw's log.
//!
//! **Readiness** stays the registry's own HTTP poll, bounded by
//! `vram.load_timeout_seconds` and nothing else: this launcher adds no
//! timeout of its own to a start. The first start of a CUDA build on a card
//! older than the build's compiled kernels JIT-compiles them first (164 s on
//! a GTX 970, Windows design §2), inside that budget; the start's log line
//! says which budget applies.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::seam::Launcher;
use super::types::{
    Exited, Found, Launch, LaunchFailure, Launched, Listed, Listing, Prepare, Prepared, Presence,
};
use crate::erased::Erased;
use crate::runtime::argv::RenderSpec;
use crate::runtime::engine_assets::{self, Engines};

pub mod engines;
mod exit;
pub mod gpu;
pub mod limits;
mod process;
pub mod ring;
mod spawner;
mod start;
mod stop;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub use ring::{LOG_LINE_BYTES, LOG_RING_LINES};

use process::Proc;

/// What [`Native`]'s `prepare` resolved for one start, carried on its
/// [`Prepared`]: the engine's executable and install, the process's
/// environment, and the start's budget for the log line.
#[derive(Debug)]
pub struct Resolved {
    class: crate::runtime::Class,
    install: engines::Install,
    env: Vec<(String, String)>,
    budget: Duration,
    /// A GPU-placed row whose run args pass no card: the engine runs on the
    /// CPU while lmgw counts the model as a GPU one (start warning).
    gpu_row_without_card: bool,
    /// What `prepare` found that the start goes on without (eSpeak NG that
    /// did not install, a card selection the build cannot apply): the
    /// start's warnings, in its log and on the entry.
    warnings: Vec<String>,
    /// The run args' limits the job of the child's own applies (Windows;
    /// [`limits`]).
    #[cfg_attr(not(windows), allow(dead_code))]
    limits: limits::Limits,
}

impl Resolved {
    /// The build directory's name of the install this start runs.
    pub(crate) fn build(&self) -> &str {
        &self.install.build
    }
}

/// The native launcher (module doc).
pub struct Native {
    /// The pinned engines under `<data>/engines`, where [`engines::resolve`]
    /// looks.
    engines: Arc<Engines>,
    /// What each name's last child runs and whether lmgw asked it to stop,
    /// for the Vulkan fallback's marks ([`exit`]).
    launched: Mutex<HashMap<String, exit::Run>>,
    /// Every child started since this launcher was made, by instance name,
    /// running or exited (an exited one keeps its log ring until the next
    /// start under its name or a remove). Never held across an await.
    children: Mutex<HashMap<String, Arc<Proc>>>,
    spawner: spawner::Spawner,
    #[cfg(windows)]
    job: windows::Job,
}

impl Native {
    /// A launcher whose engines are this platform's pinned builds, under
    /// the engines root of `data_dir` ([`engine_assets::engines_root`]).
    pub fn new(data_dir: &Path) -> std::io::Result<Self> {
        Self::with_engines(Engines::new(engine_assets::engines_root(data_dir)))
    }

    /// A launcher over `engines` (the tests: hand-installed builds, a local
    /// release server, a forced variant).
    pub fn with_engines(engines: Engines) -> std::io::Result<Self> {
        Ok(Self {
            engines: Arc::new(engines),
            launched: Mutex::new(HashMap::new()),
            children: Mutex::new(HashMap::new()),
            spawner: spawner::Spawner::new()?,
            #[cfg(windows)]
            job: windows::Job::new()?,
        })
    }

    /// Where this launcher looks for its engines.
    pub fn engines_root(&self) -> &Path {
        self.engines.root()
    }

    /// The build the next start of `class` would run
    /// ([`Engines::expected_build`]), after marking the CUDA builds whose
    /// child ended without a kernel image, as a start's prepare does first:
    /// what an image row's learned figures are keyed by
    /// (`VramScheduler::set_engine_build`).
    pub fn expected_build(&self, class: crate::runtime::Class) -> Option<String> {
        self.mark_failed_cuda_builds();
        self.engines.expected_build(class)
    }

    /// Read the machine's facts once, so [`Self::expected_build`] can choose
    /// before the first start (boot).
    pub async fn read_facts(&self) {
        self.engines.facts().await;
    }

    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Proc>>> {
        self.children.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn child(&self, name: &str) -> Option<Arc<Proc>> {
        self.table().get(name).cloned()
    }
}

/// The answer to every adoption question: nothing outlives lmgw here.
const NOTHING_ADOPTED: &str =
    "the native launcher keeps no engine across a restart of lmgw, so there is nothing to adopt";

impl Launcher for Native {
    fn runs_containers(&self) -> bool {
        false
    }

    fn prepare<'a>(&'a self, prepare: Prepare<'a>) -> Erased<'a, Result<Prepared, String>> {
        Box::pin(async move { self.resolve(prepare).await })
    }

    fn launch<'a>(&'a self, launch: Launch<'a>) -> Erased<'a, Result<Launched, LaunchFailure>> {
        Box::pin(async move { self.start(launch).await })
    }

    fn exited<'a>(&'a self, name: &'a str) -> Erased<'a, Option<Exited>> {
        Box::pin(async move { self.exit_or_fall_back(name).await })
    }

    fn stop<'a>(&'a self, name: &'a str, unload_timeout: Duration) -> Erased<'a, Option<String>> {
        Box::pin(async move { self.stop_and_wait(name, unload_timeout).await })
    }

    fn stop_quietly<'a>(&'a self, name: &'a str) -> Erased<'a, ()> {
        Box::pin(async move {
            if let Some(p) = self.child(name) {
                self.note_stop(name, &p);
                p.ask_to_stop();
            }
        })
    }

    fn remove<'a>(&'a self, name: &'a str) -> Erased<'a, Result<(), String>> {
        Box::pin(async move { self.kill_and_forget(name).await })
    }

    fn logs_tail<'a>(&'a self, name: &'a str, lines: usize) -> Erased<'a, Result<String, String>> {
        Box::pin(async move {
            match self.child(name) {
                Some(p) => Ok(p.ring.tail(lines)),
                None => Err(format!(
                    "no engine process named {name} ran since lmgw started (the native launcher \
                     keeps an engine's log in memory, until the next start under its name)"
                )),
            }
        })
    }

    fn log_excerpt<'a>(&'a self, name: &'a str) -> Erased<'a, Vec<String>> {
        Box::pin(async move {
            self.child(name)
                .map(|p| super::log_excerpt_of(&p.ring.tail(super::LOG_TAIL)))
                .unwrap_or_default()
        })
    }

    fn exists<'a>(&'a self, name: &'a str) -> Erased<'a, Presence> {
        Box::pin(async move {
            match self.child(name) {
                Some(_) => Presence::Present,
                None => Presence::Absent,
            }
        })
    }

    fn list<'a>(
        &'a self,
        _container_prefix: &'a str,
        _which: Listing,
    ) -> Erased<'a, Result<Vec<Listed>, String>> {
        // Children of this lmgw are in the registry's map already, and none
        // of a previous lmgw's survived it (module doc).
        Box::pin(async move { Ok(Vec::new()) })
    }

    fn inspect<'a>(&'a self, _name: &'a str) -> Erased<'a, Result<Found, String>> {
        Box::pin(async move { Err(NOTHING_ADOPTED.to_string()) })
    }

    fn same_start(
        &self,
        _container_prefix: &str,
        _render: &RenderSpec,
        _found: &Found,
        _labels: &HashMap<String, String>,
    ) -> Result<(), String> {
        Err(NOTHING_ADOPTED.to_string())
    }

    fn host_pids<'a>(
        &'a self,
        names: &'a [String],
    ) -> Erased<'a, Result<HashMap<String, u32>, String>> {
        Box::pin(async move {
            Ok(names
                .iter()
                .filter_map(|n| {
                    let p = self.child(n)?;
                    p.running().then(|| (n.clone(), p.pid))
                })
                .collect())
        })
    }
}
