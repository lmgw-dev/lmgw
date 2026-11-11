//! The machine check (machine-check design §1, §2.6, §4.2, WP4): whether
//! this computer can run local models, what is missing in plain words, and
//! the plan of root steps that would fix it.
//!
//! It only reads, with the user's rights, so it runs everywhere, dev
//! instances included: at boot (after the server binds, then lmgw's own
//! spec is written, §3.6.2), when asked for a fresh one (`machine_check`'s
//! `fresh`, joined while one runs; lmgw's own spec written again first when
//! its last write failed, D95), and in the background after a model start
//! that failed or worked against the verdict ([`check_in_background`],
//! D68, D96). The fix runs only from the app's own window (§3.4, §4.3: the
//! shell's `machine_apply`, WP7), which calls [`after_fix`] once the root
//! helper's steps are over.
//!
//! - [`gather`]: the reads (lmgw-machine's facts, Podman, lmgw's own spec);
//! - [`probe`]: NVML in a separate process (`lmgw --machine-probe nvml`);
//! - [`card`] §1.1, [`toolkit`] §1.2 and the plan's DTO (with [`key`], the
//!   fingerprint of a key a step writes), [`running`] §1.3, §1.5 and §1.6,
//!   [`fit`] §1.4; the verdict (§1.7) is here;
//! - [`setup`]: what a model start learns from the check (§2.5, WP5): the
//!   card and its driver read again after a start on the graphics card
//!   failed, and the last check's verdict at resolve; [`blame`]: which of
//!   §1.1's and §1.2's problems a row's start is blamed on, and that read;
//! - [`prefetch`]: the engine images begun once the verdict turns `ready`
//!   (§5, D16, WP11);
//! - [`settings_fit`]: `machine_settings_fit`, the switch to the model
//!   settings lmgw ships for this card, which §1.4's item offers (D18,
//!   WP12; [`fit`]'s `shipped`, `present` and `switch` work it out). Every
//!   published snapshot judges §1.4 and the verdict again from the last
//!   run's reading ([`MachineChecks::refit`]), so a settings change never
//!   leaves the check offering a switch worked out from the settings before
//!   it (D135);
//! - [`fixed`]: after the root helper ran (§3.5, WP7): lmgw's own spec, the
//!   check again and the ledger's probe; the plans the helper read
//!   differently ([`MachineChecks::read_differently`], D88).
//!
//! Every sentence is plain: no "container", "CDI" or "Podman" (a test holds
//! every sentence of every stated machine to that); the technical names are
//! in each item's detail. In the server image ([`container`]) the sentences
//! name the lmgw container, the host and its service where that is what to
//! act on.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lmgw_api_types::machine::{
    ItemState, MachineCheck, MachineCheckArgs, MachineFailedStart, MachineItem, MachineItemId,
    MachineSummary, MachineVerdict,
};
use lmgw_machine::cdi::own::Current;
use lmgw_machine::Facts;

use crate::runtime::cdi::CdiError;

use crate::state::SharedState;
use crate::vram::GpuMemory;

mod blame;
mod blank;
mod card;
// Windows reads its machine through `windows_check`; the Linux reads stay
// compiled there, partly unused.
#[cfg_attr(windows, allow(dead_code))]
mod container;
mod fit;
mod fixed;
// Windows reads its machine through `windows_check`; the Linux reads stay
// compiled there, partly unused.
#[cfg_attr(windows, allow(dead_code))]
mod gather;
mod key;
mod netwatch;
mod prefetch;
pub mod probe;
// Windows reads its machine through `windows_check`; the Linux reads stay
// compiled there, partly unused.
#[cfg_attr(windows, allow(dead_code))]
mod running;
mod settings_fit;
mod setup;
mod toolkit;
mod windows_check;
mod words;

pub use blame::Blamed;
pub use container::METERED_NOT_CHECKED;
pub use fixed::{after_fix, DetectProbe, Fixed};
pub use netwatch::{NetChanges, NetWatch};
pub use prefetch::{connection_metered, Metered, Prefetched};
pub use settings_fit::settings_fit;
pub use setup::{after_failed_start, after_start, problem_at_resolve};

/// How the check reads amdgpu's counters: `None` without an AMD card that
/// has them. Production's is the ledger's own [`crate::vram::AmdSysfsProbe`];
/// a test states one.
pub type AmdReader = Arc<dyn Fn() -> Option<Result<Vec<GpuMemory>, String>> + Send + Sync>;

/// The last result a run published.
struct Last {
    check: Arc<MachineCheck>,
    /// The index of the run that read it ([`MachineChecks::started`] when
    /// it began).
    run: u64,
    /// The driver, the toolkit, the packages, the device nodes and the boot
    /// as the run found them before it read anything else: the verdict is
    /// taken at resolve only while they are still so ([`setup`]).
    read: Arc<Current>,
    /// How the last write of lmgw's spec had failed when the run began
    /// (`None`: it had not): with `read`, what a failed start compares to
    /// tell whether this result still describes the machine ([`setup`]).
    spec_err: Option<CdiError>,
    /// What a start on the card can be blamed on ([`blame`]).
    blame: Arc<blame::Blame>,
    /// What §1.4 and the verdict were judged from besides the settings;
    /// `None` for a run whose reads stopped.
    basis: Option<Arc<Basis>>,
}

/// What a run read that §1.4 and the verdict are judged from besides the
/// settings: kept with its result, so a settings change judges them again
/// without reading the machine ([`MachineChecks::refit`], D135).
#[derive(Debug)]
struct Basis {
    card: card::Card,
    toolkit: Option<toolkit::Toolkit12>,
    podman_problem: bool,
    present: fit::present::Present,
    /// The server image's words (headless server design §7).
    container: bool,
}

/// `l`'s §1.4 item and its verdict judged again against `snap`, the
/// settings as they are now; `true` when that changed them.
fn rejudge(l: &mut Last, snap: &crate::config::Snapshot) -> bool {
    let Some(b) = l.basis.clone() else {
        return false;
    };
    let Some(at) = l
        .check
        .items
        .iter()
        .position(|i| i.id == MachineItemId::SettingsFit)
    else {
        return false;
    };
    let (item, outcome) = fit::judge(b.card, &b.present, snap);
    if l.check.items[at] == item {
        return false;
    }
    let mut check = MachineCheck::clone(&l.check);
    check.items[at] = item;
    let (verdict, headline) = verdict(&check.items, &b, outcome);
    check.verdict = verdict;
    check.headline = headline;
    l.check = Arc::new(check);
    true
}

/// What a failed start compares with the last result ([`Last::read`],
/// [`Last::spec_err`]), and what that result blames.
struct Described {
    blame: Arc<blame::Blame>,
    read: Arc<Current>,
    spec_err: Option<CdiError>,
}

/// The machine check of one gateway: the machine it reads, its last result,
/// and the one run at a time.
pub struct MachineChecks {
    facts: Mutex<Arc<dyn Facts>>,
    amd: Mutex<AmdReader>,
    /// Whether a machine was stated: a real one, or one a test states. A
    /// test state's blank machine reads "no graphics card", which a start
    /// that failed must not be blamed on ([`setup`]).
    stated: AtomicBool,
    last: Mutex<Option<Last>>,
    /// How many runs began reading the machine; a run's index is this count
    /// when it began. A caller that asks for a fresh check notes it first
    /// and takes a published result only from a run of that index or
    /// later: one that began reading after it asked.
    started: AtomicU64,
    one: tokio::sync::Mutex<()>,
    /// Whether a check in the background waits to run
    /// ([`check_in_background`]): one asked meanwhile is answered by it.
    queued: AtomicBool,
    /// The failed starts' reads of §1.1 and §1.2 ([`blame`]).
    blames: blame::Reads,
    /// The pre-fetch the last turn to `ready` began ([`prefetch`]), until
    /// someone waits for it.
    prefetch: Mutex<Option<tokio::task::JoinHandle<Vec<Prefetched>>>>,
    /// The running containers a confirmed settings switch could not stop
    /// (D136): §1.4 says they still run with the settings from before it,
    /// for as long as each is the same container.
    kept: Mutex<Vec<fit::present::Running>>,
    /// The plans the root helper read differently (D88, [`fixed`]): the
    /// check offers them as instructions, never the button again.
    read_differently: Mutex<Vec<String>>,
    /// The last start on the graphics card that failed because of this
    /// computer ([`Self::failed_start`], D182): every result published
    /// carries it. Taken before `last` wherever both are.
    failed: Mutex<Option<MachineFailedStart>>,
    /// How a watch on the connection begins ([`Self::watch_network`]).
    net_watch: Mutex<NetWatch>,
    /// In the server image: whether the pre-fetch said that the metered
    /// probe is not run there; it says so once ([`prefetch`]).
    metered_said: AtomicBool,
}

impl MachineChecks {
    /// The check of a gateway on the machine `facts` describes.
    pub fn on(facts: Arc<dyn Facts>, amd: AmdReader) -> Self {
        Self {
            facts: Mutex::new(facts),
            amd: Mutex::new(amd),
            stated: AtomicBool::new(true),
            last: Mutex::new(None),
            started: AtomicU64::new(0),
            one: tokio::sync::Mutex::new(()),
            queued: AtomicBool::new(false),
            blames: blame::Reads::default(),
            prefetch: Mutex::new(None),
            kept: Mutex::new(Vec::new()),
            read_differently: Mutex::new(Vec::new()),
            failed: Mutex::new(None),
            net_watch: Mutex::new(netwatch::none("this machine has no NetworkManager watch")),
            metered_said: AtomicBool::new(false),
        }
    }

    /// The root helper answered the plan `hash` with exit 2 while a check
    /// computed it again (D88): lmgw and the system read this computer
    /// differently. Every later check offers it as instructions, so nobody
    /// is asked for a password for a plan root refuses. Kept for this
    /// process; a restart offers it once more.
    pub fn read_differently(&self, hash: &str) {
        let mut set = self
            .read_differently
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !set.iter().any(|h| h == hash) {
            set.push(hash.to_string());
        }
    }

    /// The plans [`Self::read_differently`] recorded.
    fn plans_read_differently(&self) -> Vec<String> {
        self.read_differently
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// This machine, as the tray app and the headless runner read it.
    pub fn system(facts: Arc<dyn Facts>) -> Self {
        let m = Self::on(
            facts,
            Arc::new(|| {
                crate::vram::AmdSysfsProbe::detect().map(|p| {
                    use crate::vram::GpuProbe;
                    p.devices()
                })
            }),
        );
        m.set_net_watch(netwatch::nmcli());
        m
    }

    /// A machine with nothing on it: a test state's, until the test states
    /// one ([`Self::set_machine`]).
    pub fn blank() -> Self {
        let m = Self::on(Arc::new(blank::Blank), Arc::new(|| None));
        m.stated.store(false, Ordering::SeqCst);
        m
    }

    /// Test-only: read the machine `facts` describes, and amdgpu as `amd`
    /// says. `pub` for the integration tests.
    #[doc(hidden)]
    pub fn set_machine(&self, facts: Arc<dyn Facts>, amd: AmdReader) {
        *self.facts.lock().unwrap_or_else(|e| e.into_inner()) = facts;
        *self.amd.lock().unwrap_or_else(|e| e.into_inner()) = amd;
        self.stated.store(true, Ordering::SeqCst);
    }

    /// Test-only: begin a watch on the connection with `watch` (a channel
    /// the test feeds) instead of NetworkManager's monitor. `pub` for the
    /// integration tests.
    #[doc(hidden)]
    pub fn set_net_watch(&self, watch: NetWatch) {
        *self.net_watch.lock().unwrap_or_else(|e| e.into_inner()) = watch;
    }

    /// A watch on the connection: a message each time NetworkManager
    /// reports a change (find-a-model design D133), or why there is none.
    pub(crate) fn watch_network(&self) -> Result<NetChanges, String> {
        let watch = self
            .net_watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        watch()
    }

    /// Whether a machine was stated (not a test state's blank one).
    fn stated(&self) -> bool {
        self.stated.load(Ordering::SeqCst)
    }

    /// The machine the check reads.
    fn facts(&self) -> Arc<dyn Facts> {
        self.facts.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The compressed layers Podman's image store has now, by short ID
    /// ([`crate::backends::oci::short_id`]): what an image download skips,
    /// as §1.6 reads them. `podman info` within the load timeout, then the
    /// store's layer list; the error says what couldn't be read.
    pub(crate) async fn layers_here(
        &self,
        state: &crate::state::AppState,
    ) -> Result<std::collections::BTreeSet<String>, String> {
        gather::layers_here(state, self.facts()).await
    }

    /// The memory the kernel says is available to start new programs
    /// without swapping (`MemAvailable` in `/proc/meminfo`), in bytes, as
    /// it is now: the bound a header read off the network takes
    /// (find-a-model design D129). Why it could not be read, otherwise.
    pub(crate) fn memory_available(&self) -> Result<u64, String> {
        const MEMINFO: &str = "/proc/meminfo";
        let text = lmgw_machine::facts::read_text(&*self.facts(), std::path::Path::new(MEMINFO))
            .map_err(|e| format!("{MEMINFO} could not be read: {e}"))?;
        text.lines()
            .find_map(|l| l.strip_prefix("MemAvailable:"))
            // `MemAvailable:   15845960 kB`: the kernel prints kB, always.
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .and_then(|kb| kb.checked_mul(1024))
            .ok_or_else(|| format!("{MEMINFO} names no MemAvailable"))
    }

    /// How the check reads amdgpu's counters.
    fn amd_reader(&self) -> AmdReader {
        self.amd.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// What the last result blames, and the machine as its run found it.
    fn described(&self) -> Option<Described> {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|l| Described {
                blame: l.blame.clone(),
                read: l.read.clone(),
                spec_err: l.spec_err.clone(),
            })
    }

    /// The last result, if a check ran.
    pub fn last(&self) -> Option<Arc<MachineCheck>> {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|l| l.check.clone())
    }

    /// The last result, when the run that read it began at or after run
    /// index `asked`.
    fn last_since(&self, asked: u64) -> Option<Arc<MachineCheck>> {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|l| l.run >= asked)
            .map(|l| l.check.clone())
    }

    /// Publish what a run found, its §1.4 judged against `snap` (the
    /// settings now: a save during the run counts): the result as
    /// published, and `true` when the verdict turned `ready` with it — after
    /// another verdict, or as the first one.
    fn publish(&self, mut new: Last, snap: &crate::config::Snapshot) -> (Arc<MachineCheck>, bool) {
        rejudge(&mut new, snap);
        // The stamp is held across the publish, so a failed start stamped
        // meanwhile is never lost between the two (D182).
        let failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        Arc::make_mut(&mut new.check).failed_start = failed.clone();
        let check = new.check.clone();
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let was_ready = last
            .as_ref()
            .is_some_and(|l| l.check.verdict == MachineVerdict::Ready);
        let ready = new.check.verdict == MachineVerdict::Ready;
        *last = Some(new);
        (check, ready && !was_ready)
    }

    /// The settings were published as `snap`: §1.4 and the verdict of the
    /// last result judged again from its run's reading, so the switch it
    /// offers is the one these settings give (D135). `None` when that
    /// changed nothing; else the result's summary, for the live feed, and
    /// whether the verdict turned `ready` with it, which begins the
    /// pre-fetch as any turn to `ready` does. Nothing is read: the next run
    /// reads the machine.
    pub(crate) fn refit(&self, snap: &crate::config::Snapshot) -> Option<(MachineSummary, bool)> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let l = last.as_mut()?;
        let was_ready = l.check.verdict == MachineVerdict::Ready;
        if !rejudge(l, snap) {
            return None;
        }
        let turned_ready = !was_ready && l.check.verdict == MachineVerdict::Ready;
        Some((MachineSummary::from(&*l.check), turned_ready))
    }

    /// A start of `class` model `model` on the graphics card failed, and
    /// the check named this computer as why (`gpu_setup`, [`setup`]): the
    /// last result and every later one carry it as
    /// [`MachineCheck::failed_start`], whoever asked for the start, so a
    /// "Not now" said before it ends (D182). The summary to tell the live
    /// feed, once a result is there to carry it.
    pub(crate) fn failed_start(&self, class: &str, model: &str) -> Option<MachineSummary> {
        let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        let count = failed.as_ref().map_or(0, |f| f.count) + 1;
        let stamp = MachineFailedStart {
            model: model.to_string(),
            class: class.to_string(),
            at: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            count,
        };
        *failed = Some(stamp.clone());
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let l = last.as_mut()?;
        Arc::make_mut(&mut l.check).failed_start = Some(stamp);
        Some(MachineSummary::from(&*l.check))
    }

    /// Record running containers a confirmed switch could not stop (D136).
    pub(crate) fn keep(&self, running: Vec<fit::present::Running>) {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        for r in running {
            if !kept.contains(&r) {
                kept.push(r);
            }
        }
    }

    /// The recorded containers that still run (each the same container),
    /// the others forgotten.
    #[cfg_attr(windows, allow(dead_code))]
    fn kept_among(&self, running: &[fit::present::Running]) -> Vec<fit::present::Running> {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|k| running.contains(k));
        kept.clone()
    }

    /// Test-only: wait until no failed start's read and no check in the
    /// background is under way or waiting. `pub` for the integration tests.
    #[doc(hidden)]
    pub async fn settled(&self) {
        loop {
            if let Some(read) = self.blames.under_way() {
                read.await;
                continue;
            }
            let _one = self.one.lock().await;
            if !self.queued.load(Ordering::SeqCst) && self.blames.under_way().is_none() {
                return;
            }
            drop(_one);
            tokio::task::yield_now().await;
        }
    }

    /// Test-only: how many reads of §1.1 and §1.2 failed starts began
    /// ([`blame`]). `pub` for the integration tests.
    #[doc(hidden)]
    pub fn blame_reads(&self) -> u64 {
        self.blames.count()
    }

    /// Test-only: wait for the pre-fetch the last turn to `ready` began, and
    /// what it did; `None` when none began since the last wait. `pub` for
    /// the integration tests.
    #[doc(hidden)]
    pub async fn prefetched(&self) -> Option<Vec<Prefetched>> {
        let handle = self
            .prefetch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        handle.await.ok()
    }

    /// What `GET /api/status` shows: the last verdict; `None` until a check
    /// finished.
    pub fn summary(&self) -> Option<MachineSummary> {
        self.last().map(|c| MachineSummary::from(&*c))
    }

    /// How many runs read the machine so far (joined callers share one).
    pub fn runs(&self) -> u64 {
        self.started.load(Ordering::SeqCst)
    }
}

/// What one run found: the result, §1.1's conclusion, the run's index, the
/// machine and the last spec write as it began reading, and what a start
/// can be blamed on.
struct Ran {
    check: Arc<MachineCheck>,
    card: card::Card,
    index: u64,
    read: Arc<Current>,
    spec_err: Option<CdiError>,
    blame: Arc<blame::Blame>,
    basis: Option<Arc<Basis>>,
}

/// How the last write of lmgw's spec failed, if it did.
fn spec_err(state: &SharedState) -> Option<CdiError> {
    state.cdi().last().and_then(|l| l.outcome.err())
}

/// One run: read and judge the machine, under the one-run lock the caller
/// holds. Not published: the caller does that.
async fn run(state: &SharedState) -> Ran {
    let m = &state.machine;
    let index = m.started.fetch_add(1, Ordering::SeqCst);
    let facts = m.facts();
    let amd = m.amd_reader();
    // Before anything else is read: a machine that changes during the run
    // then reads as changed at resolve, never as the run found it.
    let read = Arc::new(Current::read(&*facts));
    let spec_err = spec_err(state);
    let (c, card, blame, basis) = read_and_judge(state, facts, amd).await;
    Ran {
        check: Arc::new(c),
        card,
        index,
        read,
        spec_err,
        blame: Arc::new(blame),
        basis,
    }
}

/// What [`run`] judged: the check, §1.1's conclusion, the blame, and the
/// basis §1.4 is judged again from.
type Judged = (MachineCheck, card::Card, blame::Blame, Option<Arc<Basis>>);

#[cfg(not(windows))]
async fn read_and_judge(
    state: &SharedState,
    facts: Arc<dyn lmgw_machine::Facts>,
    amd: AmdReader,
) -> Judged {
    match gather::gather(state, facts, amd).await {
        Ok(g) => {
            let render_nodes = !g.card.render_nodes.is_empty();
            let (c, basis) = judge(g);
            let blame = blame::Blame::of(&c, render_nodes);
            (c, basis.card, blame, Some(Arc::new(basis)))
        }
        Err(why) => {
            tracing::error!("the machine check could not finish: {why}");
            (
                unfinished(&why),
                card::Card::Problem,
                blame::Blame::default(),
                None,
            )
        }
    }
}

/// Windows reads its own machine (Windows design §4.4, WP6). Its §1.4 is
/// not judged, so there is no basis to judge it again from.
#[cfg(windows)]
async fn read_and_judge(
    state: &SharedState,
    facts: Arc<dyn lmgw_machine::Facts>,
    _amd: AmdReader,
) -> Judged {
    let w = windows_check::gather(state, facts).await;
    let (c, card) = windows_check::check(&w);
    let blame = blame::Blame::of(&c, false);
    (c, card, blame, None)
}

/// A run whose reads stopped (the blocking thread they run on ended
/// without an answer): a result that says so in its own words, which blames
/// no start on the machine, rather than a guess at it.
#[cfg_attr(windows, allow(dead_code))]
fn unfinished(why: &str) -> MachineCheck {
    let sentence = "lmgw couldn't finish checking this computer. The details say why.";
    let mut items = vec![words::item(
        MachineItemId::GraphicsCard,
        ItemState::Problem,
        sentence,
        vec![why.to_string()],
        None,
    )];
    items.extend(
        [
            MachineItemId::CardForModels,
            MachineItemId::RunningModels,
            MachineItemId::SettingsFit,
            MachineItemId::Room,
            MachineItemId::Download,
        ]
        .map(|id| words::not_checked(id, "the check stopped")),
    );
    MachineCheck {
        checked_at: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        verdict: MachineVerdict::NeedsSteps,
        headline: sentence.into(),
        items,
        plan: None,
        failed_start: None,
    }
}

/// Publish `ran`, and begin the pre-fetch when the verdict turned `ready`
/// with it (§5, D16): once per turn, without waiting for it.
fn publish(state: &SharedState, ran: Ran) -> Arc<MachineCheck> {
    let m = &state.machine;
    let (check, turned_ready) = m.publish(
        Last {
            check: ran.check,
            run: ran.index,
            read: ran.read,
            spec_err: ran.spec_err,
            blame: ran.blame,
            basis: ran.basis,
        },
        &state.snapshot(),
    );
    // A dashboard left open reads the check again on it (D183).
    state.telemetry.machine(MachineSummary::from(&*check));
    if turned_ready {
        begin_prefetch(state);
    }
    check
}

/// Begin the pre-fetch (§5, D16, D73) without waiting for it.
pub(crate) fn begin_prefetch(state: &SharedState) {
    let st = state.clone();
    let handle = tokio::spawn(async move { prefetch::run(&st).await });
    *state
        .machine
        .prefetch
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(handle);
}

/// The check: the last result, or (`fresh`, or none yet) a new run. A fresh
/// request joins a run only when that run began reading after the request
/// came in, so it never answers with what was read before it asked.
pub async fn check(state: &SharedState, fresh: bool) -> Arc<MachineCheck> {
    checked(state, fresh, false).await
}

/// [`check`]; `rewrite`: a person asked (D95), so a last write of lmgw's
/// spec that failed for a cause the check blames is made again first —
/// the check reads that write's outcome, and only a write updates it — and
/// a run that began after the request but without that write is not
/// joined.
async fn checked(state: &SharedState, fresh: bool, rewrite: bool) -> Arc<MachineCheck> {
    let m = &state.machine;
    if !fresh {
        if let Some(last) = m.last() {
            return last;
        }
    }
    // Not fresh: any result will do, the boot run's included.
    let asked = if fresh {
        m.started.load(Ordering::SeqCst)
    } else {
        0
    };
    let _one = m.one.lock().await;
    let rewrite = rewrite && failed_write(state).is_some();
    if !rewrite {
        if let Some(last) = m.last_since(asked) {
            return last;
        }
    }
    if rewrite {
        rewrite_failed(state).await;
    }
    let ran = run(state).await;
    publish(state, ran)
}

/// The last write of lmgw's spec, when it failed for a cause the check
/// blames on someone (§1.2, D57): the toolkit's, the card's, a folder that
/// is not private, lmgw's own files.
fn failed_write(state: &SharedState) -> Option<CdiError> {
    spec_err(state).filter(|e| toolkit::write_failed(e).is_some())
}

/// Write lmgw's spec again because its last write failed (D95), within the
/// load timeout, as a start would: what it comes to is the check's to say.
async fn rewrite_failed(state: &SharedState) {
    let Some(e) = failed_write(state) else {
        return;
    };
    tracing::info!(
        "machine check asked for: lmgw's last write of its CDI spec failed ({e}), so it is \
         written again first"
    );
    let prefix = state.snapshot().settings.container_prefix.clone();
    let _ = state
        .cdi()
        .ensure_fresh(&prefix, crate::runtime::cdi::load_timeout(state))
        .await;
}

/// A fresh check in the background (§2.6): after a failed start's read of
/// §1.1 and §1.2 ([`blame`]), or a start on the card that worked while the
/// last check said it could not ([`setup`]). One waits at a time: asked
/// while one waits to run, it is that one, which reads after it was asked;
/// asked while one reads, it is another. It writes nothing: a start on the
/// NVIDIA card writes lmgw's spec itself, just before.
pub(crate) fn check_in_background(state: &SharedState) {
    let m = &state.machine;
    if m.queued.swap(true, Ordering::SeqCst) {
        return;
    }
    let st = state.clone();
    tokio::spawn(async move {
        let m = &st.machine;
        let _one = m.one.lock().await;
        m.queued.store(false, Ordering::SeqCst);
        let c = publish(&st, run(&st).await);
        tracing::info!("machine check again: {:?}: {}", c.verdict, c.headline);
    });
}

/// `machine_check` (§4.2), the op and `lmgw__machine_check`: a fresh one
/// writes lmgw's spec again first when its last write failed (D95).
pub async fn op(state: &SharedState, args: MachineCheckArgs) -> Result<MachineCheck, String> {
    Ok((*checked(state, args.fresh, args.fresh).await).clone())
}

/// At boot, in the background after the server binds (§2.6): the check,
/// then lmgw's own spec written where an NVIDIA card's driver works
/// (§3.6.2), then the check again when that write changed what it says —
/// all under the one-run lock, and published only at the end, so the
/// status never says "ready" for a spec that is still to be written. A
/// request in the meantime waits for it (the write is bounded by the load
/// timeout) and takes its result.
pub async fn boot(state: &SharedState) {
    let m = &state.machine;
    let _one = m.one.lock().await;
    let mut ran = run(state).await;
    // Windows starts engines as programs, not containers: there is no CDI
    // spec to write (Windows design §4.1).
    if cfg!(not(windows)) && ran.card == card::Card::NvidiaOk {
        let prefix = state.snapshot().settings.container_prefix.clone();
        let outcome = state
            .cdi()
            .ensure_at_boot(&prefix, crate::runtime::cdi::load_timeout(state))
            .await;
        use crate::runtime::cdi::Ensured;
        if !matches!(
            outcome,
            Ok(Ensured::Fresh | Ensured::DefaultDirs(_) | Ensured::Unmanaged)
        ) {
            tracing::info!(
                "machine check before writing lmgw's CDI spec: {:?}: {}",
                ran.check.verdict,
                ran.check.headline
            );
            ran = run(state).await;
        }
    }
    let c = publish(state, ran);
    tracing::info!("machine check: {:?}: {}", c.verdict, c.headline);
}

/// The items and the verdict from what was read, and what §1.4 and the
/// verdict are judged from again when the settings change.
#[cfg_attr(windows, allow(dead_code))]
fn judge(g: gather::Gathered) -> (MachineCheck, Basis) {
    let container = g.deployment.is_container();
    let (card_item, card) = container::judge_card(&g.card, g.deployment);
    let (toolkit_item, plan, toolkit) = match (&g.toolkit, card) {
        (Some(t), card::Card::NvidiaOk) => {
            let (i, p, o) = container::judge_toolkit(t, g.deployment);
            (i, p, Some(o))
        }
        (_, card::Card::AmdOk) => (
            words::item(
                MachineItemId::CardForModels,
                ItemState::Ok,
                "Models can use the graphics card.",
                vec![if g.card.kfd {
                    format!(
                        "AMD cards need no extra piece: models get {} and {}",
                        gather::KFD,
                        gather::DRI
                    )
                } else {
                    format!(
                        "AMD cards need no extra piece: models get {}; there is no {}, so \
                         ROCm engines can't use this card and Vulkan ones can",
                        gather::DRI,
                        gather::KFD
                    )
                }],
                None,
            ),
            None,
            None,
        ),
        (_, card::Card::None) => (
            words::not_checked(MachineItemId::CardForModels, "no graphics card"),
            None,
            None,
        ),
        (_, card::Card::NeedsRestart) => (
            words::not_checked(MachineItemId::CardForModels, "restart first"),
            None,
            None,
        ),
        _ => (container::card_first(&card_item, container), None, None),
    };
    let (podman_item, podman_problem) = running::podman(&g.podman, container);
    // The switch to this card's settings needs no password (D18): a button
    // here, like the helper's, when it makes the settings fit (D110).
    let (fit_item, fit) = fit::judge(card, &g.present, &g.snap);
    let download = if card == card::Card::None {
        running::Download {
            item: words::not_checked(MachineItemId::Download, "no graphics card"),
            remaining: None,
        }
    } else {
        running::download(&g.images)
    };
    let room = running::room(&g.rooms, download.remaining, g.mounts.as_ref());
    let items = vec![
        card_item,
        toolkit_item,
        podman_item,
        fit_item,
        room,
        download.item,
    ];
    let basis = Basis {
        card,
        toolkit,
        podman_problem,
        present: g.present,
        container,
    };
    let (verdict, headline) = verdict(&items, &basis, fit);
    (
        MachineCheck {
            checked_at: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            verdict,
            headline,
            items,
            plan,
            // The stamp is the publish's to add (D182).
            failed_start: None,
        },
        basis,
    )
}

/// §1.7: one verdict and its headline, taken from the item that decided
/// it. A problem only instructions fix outweighs one a button fixes, since
/// the buttons alone would not make the machine ready: the headline is then
/// the first such problem's (§1.2's when the toolkit needs steps, else
/// §1.3's, else §1.4's), never a sentence that offers a button. A restart
/// comes before steps: what is wrong after it may be gone. Of the two
/// buttons, the helper's (§1.2) comes before the settings switch (§1.4), in
/// the items' order.
fn verdict(items: &[MachineItem], b: &Basis, fit: fit::Outcome) -> (MachineVerdict, String) {
    use card::Card;
    use toolkit::Toolkit12;
    let (card, toolkit, podman_problem) = (b.card, b.toolkit, b.podman_problem);
    let sentence = |id: MachineItemId| {
        items
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.sentence.clone())
            .unwrap_or_default()
    };
    match card {
        Card::None => (
            MachineVerdict::CloudOnly,
            "No graphics card found. Cloud models work; local models need a graphics card.".into(),
        ),
        Card::NeedsDriver => (
            MachineVerdict::NeedsDriver,
            "Your NVIDIA card needs NVIDIA's driver before it can run models.".into(),
        ),
        Card::NeedsRestart => (
            MachineVerdict::NeedsRestart,
            sentence(MachineItemId::GraphicsCard),
        ),
        Card::Problem => (
            MachineVerdict::NeedsSteps,
            sentence(MachineItemId::GraphicsCard),
        ),
        Card::NvidiaOk | Card::AmdOk => {
            if toolkit == Some(Toolkit12::Restart) {
                (
                    MachineVerdict::NeedsRestart,
                    sentence(MachineItemId::CardForModels),
                )
            } else if toolkit == Some(Toolkit12::Steps) {
                (
                    MachineVerdict::NeedsSteps,
                    sentence(MachineItemId::CardForModels),
                )
            } else if podman_problem {
                (
                    MachineVerdict::NeedsSteps,
                    sentence(MachineItemId::RunningModels),
                )
            } else if fit == fit::Outcome::Steps {
                (
                    MachineVerdict::NeedsSteps,
                    sentence(MachineItemId::SettingsFit),
                )
            } else if toolkit == Some(Toolkit12::Fixable) {
                (
                    MachineVerdict::NeedsFix,
                    sentence(MachineItemId::CardForModels),
                )
            } else if fit == fit::Outcome::Switch {
                (
                    MachineVerdict::NeedsFix,
                    sentence(MachineItemId::SettingsFit),
                )
            } else {
                (MachineVerdict::Ready, container::ready(b.container))
            }
        }
    }
}
