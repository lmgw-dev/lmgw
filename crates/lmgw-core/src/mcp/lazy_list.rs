//! The lazy-list contract (§9) — connect what is not `Ready` yet, wait a
//! bounded budget for it — for **every** enabled server
//! ([`McpManager::list_tools`]) or only the ones a caller named
//! ([`McpManager::list_tools_of`]).
//!
//! The targeted form is what a run's resolve takes (realtime-server-tools
//! design §1.2, both routes): a request that attaches one label connects that
//! one server, and a built-in label connects none. Idle reaping makes "not
//! ready" the normal state, so the aggregate form would otherwise wake every
//! server on the box, and wait up to [`LAZY_LIST_BUDGET`] for them, on each
//! request that carries an `mcp` tool.
//!
//! Either way the answer is the aggregate of everything `Ready`: exposed names
//! depend on every enabled server's tools (a collision prefixes both sides,
//! [`super::names`]), and it is the same map `call` routes on.

use std::time::{Duration, Instant};

use crate::config::{McpServer, Snapshot};

use super::{Aggregate, McpManager, McpStatus, LAZY_LIST_BUDGET};

impl McpManager {
    /// [`list_tools`](Self::list_tools) for the servers `ids` names only:
    /// those of them not `Ready` get a connect attempt within the same
    /// budget, and no other server is touched. An empty `ids` connects
    /// nothing and returns what is already listed.
    pub async fn list_tools_of(&self, snap: &Snapshot, ids: &[i64]) -> Aggregate {
        if !ids.is_empty() {
            self.connect_lazily(snap, |s| ids.contains(&s.id)).await;
        }
        self.aggregate(snap).await
    }

    /// Kick a connect for every enabled, listable, not-yet-`Ready` server
    /// `wanted` admits, and wait up to [`LAZY_LIST_BUDGET`] for them to
    /// settle.
    pub(super) async fn connect_lazily(
        &self,
        snap: &Snapshot,
        wanted: impl Fn(&McpServer) -> bool,
    ) {
        let dev = self.dev_urls(snap).await;
        // Which enabled servers aren't Ready yet? Those are the lazy-connect set.
        let to_connect: Vec<&McpServer> = {
            let conns = self.conns.read().await;
            snap.mcp_servers
                .values()
                .filter(|s| s.enabled)
                // A device row connects when its device does, never on a
                // list: an offline one is answered at once (§5.3).
                .filter(|s| !s.is_device())
                .filter(|s| wanted(s))
                .filter(|s| self.listable_now(s, &dev))
                .filter(|s| {
                    conns
                        .get(&s.id)
                        .map(|c| c.status != McpStatus::Ready)
                        .unwrap_or(true)
                })
                .collect()
        };
        if to_connect.is_empty() {
            return;
        }

        // Kick each lazy connect as a **detached** task, then only *wait* up
        // to the budget for readiness. Awaiting `start_one` inside a
        // `timeout` would *cancel* it on expiry — dropping the in-flight
        // connect future kills the `podman run` mid-pull (rmcp's child
        // cleanup), so a server whose cold pull exceeds the budget could
        // never become `Ready` (re-pulled from scratch every list). Detached,
        // the pull completes in the background and the server is `Ready` by
        // the client's **next** `tools/list`. `start_one`'s atomic
        // `Connecting`-claim prevents a concurrent list/tick from spawning a
        // duplicate `podman run`, and its failure bookkeeping always runs so
        // backoff engages.
        let ids: Vec<i64> = to_connect.iter().map(|s| s.id).collect();
        let mut spawned = false;
        if let Some(app) = self.app() {
            for s in &to_connect {
                let app = app.clone();
                let server = (*s).clone();
                tokio::spawn(async move { app.mcp.start_one(&server).await });
            }
            spawned = true;
        }

        // Wait up to the per-server budget for the kicked connects to
        // **settle**; the spawned tasks run on regardless of whether we keep
        // waiting.
        //
        // Settled means `Ready` or `Error` — the two states `start_one`
        // finishes in. Waiting on "left `Connecting`" instead was wrong in
        // both directions: `reconcile` parks an `autostart = false` server at
        // `Stopped`, and a server seen for the first time has no conn entry
        // at all (`tokio::spawn` only queues the task, and the uncontended
        // `conns.read()` below resolves without yielding, so this poll
        // routinely runs before `start_one` claims its slot). Either way the
        // loop broke on its first pass and returned an empty aggregate — so
        // the *first* `tools/list` reported **no tools** for every lazy
        // server, and only a second one saw them. That is precisely the wait
        // this loop exists to perform (§9 "connect on first use").
        //
        // When nothing was spawned (no `AppState` yet) a missing entry stays
        // settled, so this can never burn the full budget on a connect that
        // will not happen.
        let deadline = Instant::now() + LAZY_LIST_BUDGET;
        loop {
            let pending = {
                let conns = self.conns.read().await;
                ids.iter().any(|id| {
                    conns.get(id).map_or(spawned, |c| {
                        !matches!(c.status, McpStatus::Ready | McpStatus::Error(_))
                    })
                })
            };
            if !pending || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
