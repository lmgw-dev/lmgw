//! What was found (document-conversion design §4.3, §4.4): the snapshot of
//! every tool's probe, in memory only (D7: detection is cheap, a stored copy
//! would only go stale), and when it is taken again.
//!
//! - At start, in the background after the listener is up ([`boot`]).
//! - When asked: Settings' "Check again", the `doc_tools_check` op
//!   ([`DocTools::check`]), which joins a check that began after it was
//!   asked.
//! - On use ([`DocTools::current`]): a tool the snapshot marks missing is
//!   looked up again (a walk over `PATH`, no process), and one that was
//!   found is stat'ed again, its binary and what its extras were read from
//!   (a language pack directory's mtime, LibreOffice's launchers, the other
//!   binaries of the tool); one that appeared, went away or changed
//!   (`(dev, inode, mtime)`) is probed again before the answer, so a
//!   `dnf install` or a removal shows without a restart or a button.
//!
//! On Windows `PATH` is read again for each lookup, merged with the
//! registry's ([`super::winpath`]), because an installer changes the
//! registry, not this process. Elsewhere it is the process's, read once.
//!
//! lmgw's own reader executable is a tool like the others, looked up where
//! lmgw installs it ([`super::reader::own_dirs`]) instead of on `PATH`: a
//! program of that name somewhere on `PATH` is not lmgw's.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwapOption;
use lmgw_api_types::documents::ToolExtra;

use super::caps::{ToolId, TOOLS};
use super::probe::{self, Located};
use super::watch::Watched;
use super::winpath;

/// A found tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub located: Located,
    pub version: String,
    pub extras: Vec<ToolExtra>,
    /// What the extras were read from besides the binary (language pack
    /// directory, LibreOffice's launchers, the other binaries): compared
    /// again by [`DocTools::current`].
    pub watched: Vec<Watched>,
}

/// One tool's probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact {
    Found(Found),
    Missing,
    /// The binary is there, its probe failed: not used until a check
    /// succeeds.
    ProbeFailed {
        located: Located,
        message: String,
    },
}

impl Fact {
    fn located(&self) -> Option<&Located> {
        match self {
            Self::Found(f) => Some(&f.located),
            Self::ProbeFailed { located, .. } => Some(located),
            Self::Missing => None,
        }
    }
}

/// One tool's probe and when it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFact {
    pub fact: Fact,
    /// Unix seconds.
    pub checked_at: i64,
}

/// Every tool's probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFacts {
    pub tools: BTreeMap<ToolId, ToolFact>,
    /// When every tool was last probed: unix seconds.
    pub checked_at: i64,
}

impl ToolFacts {
    pub fn fact(&self, id: ToolId) -> Option<&Fact> {
        self.tools.get(&id).map(|t| &t.fact)
    }

    /// Nothing found: what a check of an empty `PATH` gives.
    pub fn all_missing() -> Self {
        Self {
            tools: TOOLS
                .iter()
                .map(|t| {
                    (
                        t.id,
                        ToolFact {
                            fact: Fact::Missing,
                            checked_at: 0,
                        },
                    )
                })
                .collect(),
            checked_at: 0,
        }
    }

    /// `id` marked found (a fake, for tests).
    #[cfg(test)]
    pub fn with_found(mut self, id: ToolId) -> Self {
        let located = Located {
            path: std::path::PathBuf::from("/opt/example/bin/tool"),
            stamp: probe::stamp(std::path::Path::new("/")).expect("/ exists"),
        };
        self.tools.insert(
            id,
            ToolFact {
                fact: Fact::Found(Found {
                    located,
                    version: "0.0.0-example".into(),
                    extras: Vec::new(),
                    watched: Vec::new(),
                }),
                checked_at: 0,
            },
        );
        self
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The snapshot, and when the check that took it began: one value, stored
/// when the probes have finished, so a check that is dropped midway leaves
/// both as they were.
struct Checked {
    /// When the last whole check began (a check that began after a caller
    /// asked is the one it joins).
    began: Instant,
    facts: Arc<ToolFacts>,
}

/// The gateway's detection state ([`crate::state::AppState::doc_tools`]).
pub struct DocTools {
    checked: ArcSwapOption<Checked>,
    /// One check or re-probe at a time.
    one: tokio::sync::Mutex<()>,
    /// The `PATH`-style list tools are looked up on.
    search_path: std::sync::RwLock<OsString>,
    /// Where a tool installed with lmgw is looked up instead; `None`: on
    /// the search path too (a test's directory of fakes).
    own_dirs: Option<OsString>,
    /// Merge the Windows registry's `Path` values in for each lookup.
    registry: bool,
}

impl DocTools {
    /// Looking on this process's `PATH`, as running a tool does (on Windows
    /// with the registry's, read again for each lookup).
    pub fn system() -> Self {
        Self {
            registry: true,
            own_dirs: Some(super::reader::own_dirs()),
            ..Self::on_path(std::env::var_os("PATH").unwrap_or_default())
        }
    }

    /// Looking on `search_path` only (a test's directory of fakes; empty:
    /// nothing is found).
    pub fn on_path(search_path: OsString) -> Self {
        Self {
            checked: ArcSwapOption::empty(),
            one: tokio::sync::Mutex::new(()),
            search_path: std::sync::RwLock::new(search_path),
            own_dirs: None,
            registry: false,
        }
    }

    /// [`DocTools::on_path`] that also finds the tools installed with lmgw
    /// where lmgw has them (a test that runs the real reader; the test
    /// state's, since the reader starts every tool).
    pub fn on_path_with_own(search_path: OsString) -> Self {
        Self {
            own_dirs: Some(super::reader::own_dirs()),
            ..Self::on_path(search_path)
        }
    }

    /// Look on `search_path` from now on (tests).
    pub fn set_search_path(&self, search_path: OsString) {
        *self.search_path.write().unwrap_or_else(|e| e.into_inner()) = search_path;
    }

    /// Where tools are looked up: the search path for the system's, lmgw's
    /// own directories for those installed with it.
    fn paths(&self) -> Paths {
        let system = self.search_path();
        Paths {
            own: self.own_dirs.clone().unwrap_or_else(|| system.clone()),
            system,
        }
    }

    fn search_path(&self) -> OsString {
        let base = self
            .search_path
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if self.registry {
            winpath::merged(&base)
        } else {
            base
        }
    }

    /// The last snapshot, without any re-check (`None` before the first).
    pub fn snapshot(&self) -> Option<Arc<ToolFacts>> {
        self.checked.load_full().map(|c| c.facts.clone())
    }

    /// Probe every tool now, or join a check that began after this call.
    pub async fn check(&self) -> Arc<ToolFacts> {
        let asked = Instant::now();
        let _one = self.one.lock().await;
        if let Some(c) = self.checked.load_full() {
            if c.began >= asked {
                return c.facts.clone();
            }
        }
        self.check_locked().await
    }

    /// A whole check, with `one` held by the caller.
    async fn check_locked(&self) -> Arc<ToolFacts> {
        let began = Instant::now();
        let paths = self.paths();
        let facts = Arc::new(probe_all(&paths).await);
        self.checked.store(Some(Arc::new(Checked {
            began,
            facts: facts.clone(),
        })));
        tracing::info!("document tools checked: {}", summary(&facts));
        facts
    }

    /// The snapshot, brought up to date first where a binary appeared, went
    /// away or changed since its probe; the first check when there was none.
    pub async fn current(&self) -> Arc<ToolFacts> {
        let paths = self.paths();
        if let Some(c) = self.checked.load_full() {
            if stale(&c.facts, &paths).is_empty() {
                return c.facts.clone();
            }
        }
        let _one = self.one.lock().await;
        // Again under the lock: a check that ran meanwhile (the boot check,
        // which holds it for its whole run) has them.
        let Some(c) = self.checked.load_full() else {
            return self.check_locked().await;
        };
        let stale = stale(&c.facts, &paths);
        if stale.is_empty() {
            return c.facts.clone();
        }
        let mut next = (*c.facts).clone();
        for id in stale {
            let tool = super::caps::tool(id);
            let fact = probed(tool, &paths).await;
            tracing::info!(
                "document tool {} changed since its last probe: {}",
                id.as_str(),
                fact_line(&fact)
            );
            next.tools.insert(
                id,
                ToolFact {
                    fact,
                    checked_at: now(),
                },
            );
        }
        let next = Arc::new(next);
        self.checked.store(Some(Arc::new(Checked {
            began: c.began,
            facts: next.clone(),
        })));
        next
    }
}

/// The tools whose binary, or what their extras were read from, is not what
/// their probe saw: a missing one that is on `path` now, a found (or failed)
/// one that is gone or changed.
fn stale(facts: &ToolFacts, paths: &Paths) -> Vec<ToolId> {
    TOOLS
        .iter()
        .filter(|t| {
            let path = paths.of(t);
            stale_one(facts, t, path)
        })
        .map(|t| t.id)
        .collect()
}

/// The two lists tools are looked up on.
struct Paths {
    system: OsString,
    own: OsString,
}

impl Paths {
    fn of(&self, tool: &super::caps::Tool) -> &std::ffi::OsStr {
        if tool.packages.bundled {
            &self.own
        } else {
            &self.system
        }
    }
}

fn stale_one(facts: &ToolFacts, t: &super::caps::Tool, path: &std::ffi::OsStr) -> bool {
    match facts.fact(t.id) {
        None => true,
        Some(Fact::Missing) => {
            probe::lookup(t.probe.binaries[0], path, t.probe.install_dirs).is_some()
        }
        Some(f) => {
            f.located()
                .is_none_or(|l| probe::stamp(&l.path) != Some(l.stamp))
                || matches!(f, Fact::Found(found) if found.watched.iter().any(|w| {
                    w.now(path, t.probe.install_dirs) != *w
                }))
        }
    }
}

/// Every tool, probed side by side.
/// `tool` probed, and LibreOffice's fact judged by whether it can be
/// confined (`confine::gate`).
async fn probed(tool: &super::caps::Tool, paths: &Paths) -> Fact {
    let fact = probe::probe(tool, paths.of(tool)).await;
    super::confine::gate(tool.id, fact, &paths.own).await
}

async fn probe_all(paths: &Paths) -> ToolFacts {
    let at = now();
    let facts = futures::future::join_all(TOOLS.iter().map(|t| probed(t, paths))).await;
    ToolFacts {
        tools: TOOLS
            .iter()
            .zip(facts)
            .map(|(t, fact)| {
                (
                    t.id,
                    ToolFact {
                        fact,
                        checked_at: at,
                    },
                )
            })
            .collect(),
        checked_at: at,
    }
}

fn fact_line(f: &Fact) -> String {
    match f {
        Fact::Found(f) => format!("{} at {}", f.version, f.located.path.display()),
        Fact::Missing => "missing".into(),
        Fact::ProbeFailed { message, .. } => format!("probe failed: {message}"),
    }
}

/// One line for the log: every tool and what it is.
fn summary(f: &ToolFacts) -> String {
    f.tools
        .iter()
        .map(|(id, t)| format!("{} {}", id.as_str(), fact_line(&t.fact)))
        .collect::<Vec<_>>()
        .join("; ")
}

/// At start, after the listener is up (design §4.3): never delays it.
pub async fn boot(state: &crate::state::SharedState) {
    state.doc_tools.check().await;
}

#[cfg(all(test, unix))]
mod tests;
