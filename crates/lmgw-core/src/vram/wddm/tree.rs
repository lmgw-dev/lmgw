//! The descendant walk over a process snapshot that names each process's
//! parent by PID only, as Windows' Toolhelp does ([`super::toolhelp`]).
//! Portable, so its rule is tested on every platform.
//!
//! Windows keeps a process's parent PID after the parent exits, and PIDs are
//! reused, so a process whose real parent died may name a PID that is now an
//! lmgw engine. A child counts only when it was created no earlier than the
//! parent it names; a process whose creation time is unknown (one this user
//! may not open) is not one lmgw started, and does not count, and neither
//! does anything under it.

use std::collections::{HashMap, HashSet};

/// One process in a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcRecord {
    pub pid: u32,
    /// The PID it was started by, which may since be another process's.
    pub ppid: u32,
    /// Its creation time (Windows: 100 ns ticks since 1601), or `None` when
    /// it could not be read.
    pub created: Option<u64>,
}

/// Every descendant of `root` in `records` by the rule in the module docs:
/// children, grandchildren, …, not `root` itself. Empty when `root` is not
/// in the records or its creation time is unknown.
pub fn descendants(records: &[ProcRecord], root: u32) -> Vec<u32> {
    let created: HashMap<u32, Option<u64>> = records.iter().map(|r| (r.pid, r.created)).collect();
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for r in records {
        kids.entry(r.ppid).or_default().push(r.pid);
    }
    let mut out = Vec::new();
    let mut seen: HashSet<u32> = HashSet::from([root]);
    let mut todo = vec![root];
    while let Some(p) = todo.pop() {
        let Some(Some(parent_born)) = created.get(&p).copied() else {
            continue;
        };
        for &k in kids.get(&p).map(Vec::as_slice).unwrap_or_default() {
            if seen.contains(&k) {
                continue;
            }
            if created
                .get(&k)
                .copied()
                .flatten()
                .is_some_and(|born| born >= parent_born)
            {
                seen.insert(k);
                out.push(k);
                todo.push(k);
            }
        }
    }
    out
}

/// The PIDs reachable from `root` through parent PIDs alone, `root`
/// included: the only processes whose creation time [`descendants`] can
/// need, so a snapshot reader opens those and not every process.
pub fn candidates(parents: &[(u32, u32)], root: u32) -> Vec<u32> {
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, ppid) in parents {
        kids.entry(ppid).or_default().push(pid);
    }
    let mut out = vec![root];
    let mut seen: HashSet<u32> = HashSet::from([root]);
    let mut todo = vec![root];
    while let Some(p) = todo.pop() {
        for &k in kids.get(&p).map(Vec::as_slice).unwrap_or_default() {
            if seen.insert(k) {
                out.push(k);
                todo.push(k);
            }
        }
    }
    out
}
