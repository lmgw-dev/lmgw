//! Which part of the GPU's used memory is lmgw's own — per-process
//! attribution for the outside-VRAM fallback (candidate-aliases design §4.7).
//!
//! The verdict §4.7 wants is "is the memory that is short memory lmgw could
//! free itself?". The ledger's estimates cannot answer that: they are lower
//! bounds (weights + KV, no compute buffers), and an underestimated lmgw
//! share reads as outside pressure — which would send to the cloud a request
//! lmgw could have served. So the share is *measured*: the driver's
//! per-process figures ([`GpuProbe::processes`](super::GpuProbe::processes))
//! for the processes of lmgw's own containers.
//!
//! **Container → processes.** A container's init process on the host is
//! `podman inspect`'s `State.Pid` (measured 2026-09-27 under rootless podman:
//! for a chat container that *is* llama-server, and it is the PID NVML lists).
//! The image class runs with `--init`, so there `State.Pid` is catatonit and
//! the process on the GPU is its child — hence every descendant counts too,
//! read from `/proc` ([`ProcTree`]).
//!
//! **Asked once per container generation.** A `podman inspect` is a process
//! spawn, so its answer is cached against the registry entry's generation
//! (a re-run container is a new entry, a new generation, a new PID) and a
//! pass that has every PID cached spawns nothing. A verdict retries whatever
//! is not known yet, in one batched `podman inspect n1 n2 …`, and a read
//! already in flight is joined rather than started again ([`Reading`]); the
//! status view asks at most once per generation and otherwise reports "not
//! read yet" ([`Fill`]). An answer is kept only for a generation still in the
//! registry when podman answers: the container's name outlives the
//! generation, so an inspect that queued behind a stop and a re-run of the
//! same name answers with the next generation's PID.
//!
//! **Just-stopped containers.** An evicted container leaves the registry the
//! moment `podman wait` returns, which can be before the driver stops listing
//! its process. Right after an eviction that memory would read as outside
//! use. So a generation that leaves keeps its processes as tombstones, still
//! counted as lmgw's for as long as the driver lists them, and dropped the
//! first time it does not.
//!
//! **Conservative by construction.** Anything that cannot be attributed —
//! a container still starting, one whose processes the driver does not list
//! (a CPU-only row, a container that died), a figure the driver cannot give —
//! makes the whole share unavailable, and the verdict falls back to today's
//! behaviour. A wrong "external" sends a request to the cloud that lmgw could
//! have served; a wrong "unavailable" only costs today's queueing.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::nvml::ProcessMemory;

// ---------------------------------------------------------------------------
// The process tree
// ---------------------------------------------------------------------------

/// Where attribution reads the host's process tree.
pub trait ProcTree: Send + Sync + 'static {
    /// Every descendant of `pid` — children, grandchildren, … — not `pid`
    /// itself. Empty when it has none, or is gone.
    fn descendants(&self, pid: u32) -> Vec<u32>;
}

/// The real process tree, from `/proc`.
///
/// Walked **down** from each container's init process: through
/// `/proc/<pid>/task/<tid>/children`, which is a handful of reads per
/// container. A kernel built without `CONFIG_PROC_CHILDREN` has no such files;
/// then one scan of `/proc/*/stat` builds the parent map instead.
pub struct ProcFs {
    root: PathBuf,
}

impl Default for ProcFs {
    fn default() -> Self {
        Self::at("/proc")
    }
}

impl ProcFs {
    /// Against an injected tree — for the tests.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The children of every thread of `pid`. `None` when this kernel has no
    /// `children` files at all.
    fn children(&self, pid: u32) -> Option<Vec<u32>> {
        let task = self.root.join(pid.to_string()).join("task");
        let Ok(threads) = std::fs::read_dir(&task) else {
            // Gone: no children.
            return Some(Vec::new());
        };
        let mut out = Vec::new();
        for t in threads.flatten() {
            match std::fs::read_to_string(t.path().join("children")) {
                Ok(text) => out.extend(
                    text.split_whitespace()
                        .filter_map(|p| p.parse::<u32>().ok()),
                ),
                // The thread is there and the file is not: the kernel does
                // not publish children.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && t.path().is_dir() => {
                    return None
                }
                // The thread exited between the listing and the read.
                Err(_) => {}
            }
        }
        Some(out)
    }

    /// `ppid -> children`, from one scan of `/proc/*/stat`.
    fn parent_map(&self) -> HashMap<u32, Vec<u32>> {
        let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return map;
        };
        for e in entries.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
                continue;
            };
            if let Some(ppid) = stat_ppid(&stat) {
                map.entry(ppid).or_default().push(pid);
            }
        }
        map
    }
}

/// The parent PID from a `/proc/<pid>/stat` line. The command name is in
/// parentheses and may itself contain spaces and parentheses, so the fields
/// are read after the *last* `)`: state, then ppid.
fn stat_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

impl ProcTree for ProcFs {
    fn descendants(&self, pid: u32) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        let mut seen: HashSet<u32> = HashSet::from([pid]);
        let mut todo = vec![pid];
        // Once the kernel turned out to have no `children` files, the one
        // scan answers for the rest of the walk.
        let mut scanned: Option<HashMap<u32, Vec<u32>>> = None;
        while let Some(p) = todo.pop() {
            let kids = match scanned
                .as_ref()
                .map(|m| m.get(&p).cloned().unwrap_or_default())
            {
                Some(k) => k,
                None => match self.children(p) {
                    Some(k) => k,
                    None => scanned
                        .insert(self.parent_map())
                        .get(&p)
                        .cloned()
                        .unwrap_or_default(),
                },
            };
            for k in kids {
                if seen.insert(k) {
                    out.push(k);
                    todo.push(k);
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The PID cache
// ---------------------------------------------------------------------------

/// Who is asking, which decides how hard a pass tries to learn a PID it does
/// not have yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// An admission verdict: it runs only for a request that has a fallback
    /// and a model that is not up, and it re-asks podman for anything not
    /// known yet — a container whose inspect failed a minute ago may answer
    /// now. A read already in flight is waited for, not asked again.
    Verdict,
    /// A verdict taken again while its request waits in the queue, every
    /// poll (review finding 8): like [`Self::Verdict`] it waits for a read
    /// in flight and asks about a container never asked about, but it does
    /// not re-ask podman for a read that failed — that would be a process
    /// spawn per poll. The next request's first verdict retries it.
    Recheck,
    /// The status view, built on every `vram` frame: asks podman at most once
    /// per container generation and otherwise reads only what is cached.
    View,
}

/// One registry entry, as a pass sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub generation: u64,
    /// `class/model`, for every sentence a pass writes.
    pub label: String,
    pub name: String,
}

/// A container's measured processes: its init PID and what is under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub generation: u64,
    pub label: String,
    /// The init PID first, then its descendants.
    pub pids: Vec<u32>,
}

/// A `podman inspect` in flight (review finding 4): done once its answer is
/// recorded, which is when the task running it drops the matching
/// [`ReadDone`]. Every pass that finds a generation pending waits on the
/// same read instead of spawning another.
#[derive(Debug, Clone)]
pub struct Reading(tokio::sync::watch::Receiver<()>);

impl Reading {
    /// Until the read's answer is recorded.
    pub async fn done(mut self) {
        // Nothing is ever sent: the only event is the sender going away, and
        // `changed` reports that even if it happened before this call.
        let _ = self.0.changed().await;
    }
}

/// Held by whoever runs the inspect a [`Plan`] asks for; dropping it — after
/// the answer is recorded — completes the [`Reading`]s waiting on it.
#[derive(Debug)]
pub struct ReadDone(#[allow(dead_code)] tokio::sync::watch::Sender<()>);

#[derive(Debug, Clone)]
enum Slot {
    /// A `podman inspect` for it is running.
    Pending(Reading),
    /// `State.Pid`, and the process tree last measured under it — what a
    /// tombstone is made of once the generation leaves the registry.
    Known {
        label: String,
        root: u32,
        tree: Vec<u32>,
    },
    /// podman answered without a running PID, or could not be asked: why.
    Failed(String),
}

/// A process of a container that has left the registry, still counted as
/// lmgw's while the driver lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub pid: u32,
    pub label: String,
}

/// What a pass does before it can attribute anything.
#[derive(Debug, Default)]
pub struct Plan {
    /// Containers whose init PID is known.
    pub known: Vec<(Container, u32)>,
    /// Containers to ask podman about, in one batched `inspect`. Marked
    /// pending in the cache already.
    pub inspect: Vec<Container>,
    /// For whoever runs [`Self::inspect`]: dropped once the answer is
    /// recorded. `Some` exactly when `inspect` is not empty.
    pub done: Option<ReadDone>,
    /// The reads a verdict waits for before it attributes: its own, and any
    /// already in flight for a generation it needs. Empty for the view,
    /// which never waits.
    pub wait: Vec<Reading>,
    /// Why this pass cannot attribute everything even after the inspect —
    /// the status view's "not read yet". The inspect still runs, so the next
    /// pass has the PID.
    pub blocked: Option<String>,
}

/// Per-generation `State.Pid`s and the tombstones of generations that left.
#[derive(Debug, Default)]
pub struct PidCache {
    slots: HashMap<u64, Slot>,
    tombstones: Vec<Tombstone>,
}

impl PidCache {
    /// Retire every generation that is no longer in `current` into
    /// tombstones, then sort `current` into known PIDs and PIDs to ask for.
    pub fn plan(&mut self, current: &[Container], fill: Fill) -> Plan {
        let live: HashSet<u64> = current.iter().map(|c| c.generation).collect();
        let gone: Vec<u64> = self
            .slots
            .keys()
            .filter(|g| !live.contains(g))
            .copied()
            .collect();
        for g in gone {
            if let Some(Slot::Known { label, root, tree }) = self.slots.remove(&g) {
                for pid in std::iter::once(root).chain(tree) {
                    if !self.tombstones.iter().any(|t| t.pid == pid) {
                        self.tombstones.push(Tombstone {
                            pid,
                            label: label.clone(),
                        });
                    }
                }
            }
        }

        let mut plan = Plan::default();
        let mut read: Option<Reading> = None;
        for c in current {
            let ask = match (self.slots.get(&c.generation), fill) {
                (Some(Slot::Known { root, .. }), _) => {
                    plan.known.push((c.clone(), *root));
                    false
                }
                (None, _) | (Some(Slot::Failed(_)), Fill::Verdict) => true,
                // Joined, never asked again: N requests right after a start
                // share one inspect rather than queueing N behind podman's
                // locks (review finding 4).
                (Some(Slot::Pending(r)), Fill::Verdict | Fill::Recheck) => {
                    plan.wait.push(r.clone());
                    false
                }
                (Some(Slot::Pending(_)), Fill::View) => {
                    plan.blocked.get_or_insert_with(|| {
                        format!("{}: its container's PID is being read", c.label)
                    });
                    false
                }
                (Some(Slot::Failed(why)), Fill::View | Fill::Recheck) => {
                    plan.blocked.get_or_insert_with(|| why.clone());
                    false
                }
            };
            if ask {
                let reading = read.get_or_insert_with(|| {
                    let (tx, rx) = tokio::sync::watch::channel(());
                    plan.done = Some(ReadDone(tx));
                    Reading(rx)
                });
                self.slots
                    .insert(c.generation, Slot::Pending(reading.clone()));
                plan.inspect.push(c.clone());
            }
        }
        if let (Some(r), Fill::Verdict | Fill::Recheck) = (read, fill) {
            plan.wait.push(r);
        }
        if fill == Fill::View && !plan.inspect.is_empty() {
            // The view does not wait for podman to answer: this frame says
            // "not read yet", the next one has it.
            plan.blocked.get_or_insert_with(|| {
                format!(
                    "{}: its container's PID is being read",
                    plan.inspect[0].label
                )
            });
        }
        plan
    }

    /// Record what one batched inspect answered for the containers it asked
    /// about. `live` is every generation in the registry when podman
    /// answered: a generation that left the registry meanwhile is not
    /// revived, and its answer is dropped — the name it was asked by may
    /// already be the next generation's container, whose PID a tombstone
    /// would then count twice (review finding 5).
    pub fn record(
        &mut self,
        asked: &[Container],
        answer: &Result<HashMap<String, u32>, String>,
        live: &HashSet<u64>,
    ) {
        for c in asked {
            if !live.contains(&c.generation) {
                self.slots.remove(&c.generation);
                continue;
            }
            let Some(slot) = self.slots.get_mut(&c.generation) else {
                continue;
            };
            *slot = match answer {
                Ok(pids) => match pids.get(&c.name) {
                    Some(&pid) if pid > 0 => Slot::Known {
                        label: c.label.clone(),
                        root: pid,
                        tree: Vec::new(),
                    },
                    _ => Slot::Failed(format!(
                        "{}: podman reports no running process for container '{}'",
                        c.label, c.name
                    )),
                },
                Err(e) => Slot::Failed(format!(
                    "{}: reading its container's PID failed: {e}",
                    c.label
                )),
            };
        }
    }

    /// The init PID of one generation, or why it is not known.
    pub fn root(&self, generation: u64) -> Result<u32, String> {
        match self.slots.get(&generation) {
            Some(Slot::Known { root, .. }) => Ok(*root),
            Some(Slot::Failed(why)) => Err(why.clone()),
            Some(Slot::Pending(_)) => Err("a container's PID is still being read".into()),
            None => Err("a container's PID has not been read".into()),
        }
    }

    /// Keep the tree each member was measured with, so a generation that
    /// leaves takes its current processes into its tombstones.
    pub fn remember(&mut self, members: &[Member]) {
        for m in members {
            if let Some(Slot::Known { tree, .. }) = self.slots.get_mut(&m.generation) {
                *tree = m.pids.iter().skip(1).copied().collect();
            }
        }
    }

    /// Every tombstoned PID, for the probe to be asked about.
    pub fn tombstone_pids(&self) -> Vec<u32> {
        self.tombstones.iter().map(|t| t.pid).collect()
    }

    /// What the tombstones still hold, by the driver's answer. `asked` is
    /// every PID the driver was asked about in this pass: a tombstone it was
    /// asked about and no longer lists is dropped for good. One it was not
    /// asked about — retired by a concurrent pass after this one read the
    /// tombstone list — is left alone: a probe that answers only for the PIDs
    /// it is given (amdgpu) says nothing about it (review finding 3).
    pub fn settle(
        &mut self,
        processes: &[ProcessMemory],
        asked: &HashSet<u32>,
    ) -> Result<u64, String> {
        let listed: HashMap<u32, Option<u64>> =
            processes.iter().map(|p| (p.pid, p.bytes)).collect();
        self.tombstones
            .retain(|t| listed.contains_key(&t.pid) || !asked.contains(&t.pid));
        let mut total: u64 = 0;
        for t in &self.tombstones {
            match listed.get(&t.pid) {
                // Not asked about in this pass.
                None => {}
                Some(Some(b)) => total = total.saturating_add(*b),
                Some(None) => {
                    return Err(format!(
                        "{} was stopped and the driver still lists its process {} without \
                         saying how much it holds",
                        t.label, t.pid
                    ))
                }
            }
        }
        Ok(total)
    }
}

/// lmgw's share held by its registered containers: every member's processes
/// as the driver lists them.
///
/// `Err` names the first member that cannot be attributed — none of its
/// processes is listed (a CPU-only row, a container that died, a driver that
/// has not caught up with a fresh start), or one is listed without a figure.
/// Either way the share would be a guess, and a low guess is exactly the
/// wrong direction (module docs).
pub fn members_share(members: &[Member], processes: &[ProcessMemory]) -> Result<u64, String> {
    let listed: HashMap<u32, Option<u64>> = processes.iter().map(|p| (p.pid, p.bytes)).collect();
    let mut total: u64 = 0;
    for m in members {
        let mut found = false;
        for pid in &m.pids {
            match listed.get(pid) {
                None => {}
                Some(Some(b)) => {
                    found = true;
                    total = total.saturating_add(*b);
                }
                Some(None) => {
                    return Err(format!(
                        "{} is not attributed: the driver lists its process {pid} without \
                         saying how much memory it holds",
                        m.label
                    ))
                }
            }
        }
        if !found {
            return Err(format!(
                "{} is not attributed: the driver lists none of its processes ({}) — a \
                 model running without the GPU, or a container that is gone",
                m.label,
                m.pids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pm(pid: u32, bytes: Option<u64>) -> ProcessMemory {
        ProcessMemory { pid, bytes }
    }

    fn c(generation: u64, model: &str) -> Container {
        Container {
            generation,
            label: format!("chat/{model}"),
            name: format!("lmgw-chat-{model}"),
        }
    }

    fn live(generations: &[u64]) -> HashSet<u64> {
        generations.iter().copied().collect()
    }

    fn member(generation: u64, model: &str, pids: &[u32]) -> Member {
        Member {
            generation,
            label: format!("chat/{model}"),
            pids: pids.to_vec(),
        }
    }

    // --- the process tree --------------------------------------------------

    fn write(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The image class's shape: catatonit (the container's init) with
    /// sd-server under it, found through every thread's `children` file.
    #[test]
    fn descendants_follow_every_threads_children() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        write(&p.join("100/task/100/children"), "101 ");
        write(&p.join("101/task/101/children"), "");
        // sd-server's second thread forked a helper.
        write(&p.join("101/task/105/children"), "102");
        write(&p.join("102/task/102/children"), "");
        let tree = ProcFs::at(p);
        let mut d = tree.descendants(100);
        d.sort();
        assert_eq!(d, vec![101, 102]);
        assert!(tree.descendants(999).is_empty(), "a gone process has none");
    }

    /// A kernel without `CONFIG_PROC_CHILDREN`: the parent map from `stat`,
    /// whose command names may hold spaces and parentheses.
    #[test]
    fn without_children_files_the_stat_scan_answers() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for pid in ["100", "101", "102", "200"] {
            std::fs::create_dir_all(p.join(pid).join("task").join(pid)).unwrap();
        }
        write(&p.join("100/stat"), "100 (catatonit) S 90 100 100 0 -1");
        write(
            &p.join("101/stat"),
            "101 (sd server (x)) R 100 100 100 0 -1",
        );
        write(&p.join("102/stat"), "102 (worker) S 101 100 100 0 -1");
        write(&p.join("200/stat"), "200 (other) S 1 200 200 0 -1");
        let mut d = ProcFs::at(p).descendants(100);
        d.sort();
        assert_eq!(d, vec![101, 102]);
        assert_eq!(stat_ppid("7 (a) b) S 3 7"), Some(3));
    }

    // --- the cache ---------------------------------------------------------

    /// First pass asks podman; once recorded, a pass asks nothing.
    #[test]
    fn a_known_pid_is_never_asked_for_again() {
        let mut cache = PidCache::default();
        let current = [c(1, "a"), c(2, "b")];
        let plan = cache.plan(&current, Fill::Verdict);
        assert_eq!(plan.inspect, current.to_vec());
        assert!(plan.known.is_empty() && plan.blocked.is_none());

        cache.record(
            &plan.inspect,
            &Ok(HashMap::from([
                ("lmgw-chat-a".into(), 11),
                ("lmgw-chat-b".into(), 22),
            ])),
            &live(&[1, 2]),
        );
        let plan = cache.plan(&current, Fill::Verdict);
        assert!(plan.inspect.is_empty(), "{plan:?}");
        assert_eq!(plan.known, vec![(c(1, "a"), 11), (c(2, "b"), 22)]);
        let plan = cache.plan(&current, Fill::View);
        assert!(plan.inspect.is_empty() && plan.blocked.is_none());
    }

    /// The view asks once per generation, never waits for the answer, and
    /// never retries a failure; a verdict retries it.
    #[test]
    fn the_view_asks_once_and_a_verdict_retries_what_failed() {
        let mut cache = PidCache::default();
        let current = [c(1, "a")];
        let plan = cache.plan(&current, Fill::View);
        assert_eq!(plan.inspect.len(), 1);
        assert!(plan.blocked.as_deref().unwrap().contains("being read"));
        // Still pending: the view does not ask twice.
        let again = cache.plan(&current, Fill::View);
        assert!(again.inspect.is_empty());
        assert!(again.blocked.as_deref().unwrap().contains("chat/a"));

        // podman answered, but the container has no running process.
        cache.record(
            &plan.inspect,
            &Ok(HashMap::from([("lmgw-chat-a".into(), 0)])),
            &live(&[1]),
        );
        let view = cache.plan(&current, Fill::View);
        assert!(
            view.inspect.is_empty(),
            "a failure is not re-asked by the view"
        );
        assert!(
            view.blocked
                .as_deref()
                .unwrap()
                .contains("no running process"),
            "{view:?}"
        );
        let recheck = cache.plan(&current, Fill::Recheck);
        assert!(
            recheck.inspect.is_empty() && recheck.blocked.is_some(),
            "a verdict re-taken while waiting does not spawn podman every poll"
        );
        let verdict = cache.plan(&current, Fill::Verdict);
        assert_eq!(verdict.inspect, current.to_vec(), "a verdict asks again");

        cache.record(
            &verdict.inspect,
            &Err("podman could not be run".into()),
            &live(&[1]),
        );
        let view = cache.plan(&current, Fill::View);
        assert!(view.blocked.unwrap().contains("podman could not be run"));
    }

    /// A container that left the registry keeps its processes as
    /// tombstones — counted while listed, dropped once not.
    #[test]
    fn tombstones_count_while_listed_and_drop_once_not() {
        let mut cache = PidCache::default();
        let plan = cache.plan(&[c(1, "a"), c(2, "b")], Fill::Verdict);
        cache.record(
            &plan.inspect,
            &Ok(HashMap::from([
                ("lmgw-chat-a".into(), 11),
                ("lmgw-chat-b".into(), 22),
            ])),
            &live(&[1, 2]),
        );
        // The image-class shape: the init PID and a child on the GPU.
        cache.remember(&[member(1, "a", &[11, 12]), member(2, "b", &[22])]);

        // `a` is evicted: it leaves the registry.
        let plan = cache.plan(&[c(2, "b")], Fill::Verdict);
        assert_eq!(plan.known, vec![(c(2, "b"), 22)]);
        let mut pids = cache.tombstone_pids();
        pids.sort();
        assert_eq!(pids, vec![11, 12]);

        // The driver still lists its child: still lmgw's.
        let all = HashSet::from([11, 12, 22]);
        let listed = [pm(12, Some(500)), pm(22, Some(100)), pm(900, Some(7))];
        assert_eq!(cache.settle(&listed, &all), Ok(500));
        assert_eq!(
            cache.tombstone_pids(),
            vec![12],
            "11 is not listed: dropped"
        );

        // Gone from the list: dropped for good, even if the PID came back.
        assert_eq!(cache.settle(&[pm(22, Some(100))], &all), Ok(0));
        assert!(cache.tombstone_pids().is_empty());
        assert_eq!(cache.settle(&[pm(12, Some(500))], &all), Ok(0));

        // A tombstone the driver cannot size is unknown, not zero.
        let mut cache = PidCache::default();
        let plan = cache.plan(&[c(3, "x")], Fill::Verdict);
        cache.record(
            &plan.inspect,
            &Ok(HashMap::from([("lmgw-chat-x".into(), 33)])),
            &live(&[3]),
        );
        cache.plan(&[], Fill::Verdict);
        assert!(cache
            .settle(&[pm(33, None)], &HashSet::from([33]))
            .unwrap_err()
            .contains("chat/x"));
    }

    /// Review finding 3: a probe that answers only for the PIDs it is asked
    /// about (amdgpu) says nothing about a tombstone retired after the pass
    /// read the tombstone list. That tombstone is left for the next pass, not
    /// dropped as "no longer listed".
    #[test]
    fn a_tombstone_the_pass_did_not_ask_about_is_kept() {
        let mut cache = PidCache::default();
        let plan = cache.plan(&[c(1, "a"), c(2, "b")], Fill::Verdict);
        cache.record(
            &plan.inspect,
            &Ok(HashMap::from([
                ("lmgw-chat-a".into(), 11),
                ("lmgw-chat-b".into(), 22),
            ])),
            &live(&[1, 2]),
        );
        // This pass asked about `a` and `b` as live members…
        let asked = HashSet::from([11, 22]);
        // …and a concurrent pass retires `b` before this one settles.
        cache.plan(&[c(1, "a")], Fill::Verdict);
        assert_eq!(cache.tombstone_pids(), vec![22]);
        // amdgpu's answer for what this pass asked: `b`'s process is gone.
        assert_eq!(cache.settle(&[pm(11, Some(10))], &asked), Ok(0));
        assert!(cache.tombstone_pids().is_empty(), "asked and not listed");

        // A tombstone this pass never asked about survives its settle.
        let plan = cache.plan(&[c(3, "c")], Fill::Verdict);
        cache.record(
            &plan.inspect,
            &Ok(HashMap::from([("lmgw-chat-c".into(), 33)])),
            &live(&[3]),
        );
        cache.plan(&[], Fill::Verdict);
        assert_eq!(cache.settle(&[], &HashSet::from([11])), Ok(0));
        assert_eq!(cache.tombstone_pids(), vec![33], "not asked: kept");
        assert_eq!(
            cache.settle(&[pm(33, Some(500))], &HashSet::from([33])),
            Ok(500)
        );
    }

    /// A generation retired while its inspect was running is not revived.
    #[test]
    fn an_answer_for_a_retired_generation_is_dropped() {
        let mut cache = PidCache::default();
        let plan = cache.plan(&[c(1, "a")], Fill::Verdict);
        cache.plan(&[], Fill::Verdict);
        cache.record(
            &plan.inspect,
            &Ok(HashMap::from([("lmgw-chat-a".into(), 11)])),
            &live(&[1]),
        );
        assert!(cache.root(1).is_err());
    }

    /// Review finding 5: an inspect that queued behind `stop` and `run
    /// --replace` of the same name answers with the *next* generation's PID.
    /// The generation that asked left the registry before that re-run could
    /// start, so an answer for a generation no longer registered when podman
    /// answers is dropped — no slot, no tombstone holding a live PID.
    #[test]
    fn an_answer_for_a_generation_no_longer_registered_is_dropped() {
        let mut cache = PidCache::default();
        let plan = cache.plan(&[c(1, "a")], Fill::Verdict);
        // Generation 1 was stopped and generation 2 of the same name runs
        // now; no pass ran in between, so 1's slot is still pending.
        cache.record(
            &plan.inspect,
            &Ok(HashMap::from([("lmgw-chat-a".into(), 99)])),
            &live(&[2]),
        );
        assert!(cache.root(1).is_err());
        cache.plan(&[c(2, "a")], Fill::View);
        assert!(
            cache.tombstone_pids().is_empty(),
            "generation 2's PID is not a tombstone of generation 1"
        );
    }

    /// Review finding 4: a verdict joins a read already in flight instead of
    /// asking podman again, and the read completes when its answer is
    /// recorded.
    #[tokio::test]
    async fn a_verdict_joins_a_read_in_flight() {
        let mut cache = PidCache::default();
        let first = cache.plan(&[c(1, "a")], Fill::Verdict);
        assert_eq!(first.inspect.len(), 1);
        assert_eq!(
            first.wait.len(),
            1,
            "the asking verdict waits for its own read"
        );
        let second = cache.plan(&[c(1, "a")], Fill::Verdict);
        assert!(second.inspect.is_empty(), "not asked twice");
        assert!(second.done.is_none());
        assert_eq!(second.wait.len(), 1, "it waits on the first read");

        let waiting = tokio::spawn(async move {
            for r in second.wait {
                r.done().await;
            }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        cache.record(
            &first.inspect,
            &Ok(HashMap::from([("lmgw-chat-a".into(), 11)])),
            &live(&[1]),
        );
        drop(first.done);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("the joined read completes once recorded")
            .unwrap();
        assert_eq!(cache.root(1), Ok(11));
    }

    // --- the share ---------------------------------------------------------

    #[test]
    fn the_share_sums_every_members_listed_processes() {
        let members = [member(1, "a", &[11, 12]), member(2, "b", &[22])];
        let listed = [
            pm(11, Some(10)),
            pm(12, Some(1000)),
            pm(22, Some(300)),
            // Outside: the compositor.
            pm(5, Some(400)),
        ];
        assert_eq!(members_share(&members, &listed), Ok(1310));
        assert_eq!(members_share(&[], &listed), Ok(0));
    }

    /// A member none of whose processes the driver lists — a CPU-only row —
    /// makes the share unavailable, and says which model.
    #[test]
    fn an_unlisted_member_is_named() {
        let members = [member(1, "a", &[11]), member(2, "cpu-only", &[22, 23])];
        let err = members_share(&members, &[pm(11, Some(10))]).unwrap_err();
        assert!(
            err.contains("chat/cpu-only") && err.contains("22, 23"),
            "{err}"
        );
    }

    #[test]
    fn a_member_the_driver_cannot_size_is_named() {
        let err = members_share(&[member(1, "a", &[11])], &[pm(11, None)]).unwrap_err();
        assert!(err.contains("chat/a") && err.contains("11"), "{err}");
    }
}
