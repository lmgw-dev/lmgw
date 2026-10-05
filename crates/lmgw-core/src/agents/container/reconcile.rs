//! Boot reconciliation (container-runtime design §6.4): every agent
//! container and run directory whose run is not a live job is collected.
//! A child of `container` since the chat-voice WP11 review (n1): the
//! runner's file had grown past 2000 lines.

use std::path::{Path, PathBuf};

use crate::runtime::slug;
use crate::state::SharedState;

use super::{KIND_AGENT, LABEL_INSTANCE, LABEL_KIND, LABEL_RUN};

/// What one reconciliation pass did. `listed = false` means podman could not
/// answer, which is a different thing from "nothing was left over" and is never
/// reported as success.
#[derive(Debug, Default, PartialEq)]
pub struct Reconciled {
    pub listed: bool,
    pub removed: Vec<String>,
    pub swept_dirs: Vec<String>,
    pub errors: Vec<String>,
}

/// Collect every agent container and run directory whose run is not a live job
/// (§6.4).
///
/// **An agent container is never adopted, only collected**: there is nothing to
/// adopt, since `fail_orphaned_jobs` has already failed the job that owned it.
/// That is the one difference from `Registry::reconcile`, and it is why this
/// does not share its map.
pub async fn reconcile(state: &SharedState) -> Reconciled {
    reconcile_since(state, state.started_at_utc).await
}

/// The boot pass: [`reconcile()`], logged. `fail_orphaned_jobs` has already
/// failed every run a restart interrupted, so every container carrying
/// `lmgw.kind=agent` under this instance's prefix is a leftover.
///
/// Spawned by `server::run` beside `lifecycle::boot`, never from
/// `AppState::init`: it lists and removes by `container_prefix`, and only the
/// entry point's dev-instance safety step (which runs between the two) makes
/// a fresh data dir's prefix its own (chat-voice WP5 review B1). Spawned
/// rather than awaited, for the reason `lifecycle::boot` is: `podman ps` is
/// unbounded wall clock on a box where podman is slow or absent.
pub async fn boot_reconcile(state: &SharedState) {
    let report = reconcile(state).await;
    for e in &report.errors {
        tracing::warn!("agent container reconciliation: {e}");
    }
    if !report.removed.is_empty() || !report.swept_dirs.is_empty() {
        tracing::info!(
            "agent container reconciliation: removed {} container(s), swept {} run \
             director(ies)",
            report.removed.len(),
            report.swept_dirs.len()
        );
    }
}

/// [`reconcile()`], with "when this process started" handed in.
///
/// The seam a test needs, and the only reason this is not private: a leftover
/// is *by definition* older than the process collecting it, and no test can
/// make podman backdate a container it created a moment ago. Production has
/// exactly one caller and it passes [`AppState::started_at_utc`](crate::state::AppState).
pub async fn reconcile_since(
    state: &SharedState,
    born: chrono::DateTime<chrono::Utc>,
) -> Reconciled {
    let mut report = Reconciled::default();
    let prefix = state.snapshot().settings.container_prefix.clone();
    let registry = state.runtime();
    let filters = vec![
        format!("label={LABEL_KIND}={KIND_AGENT}"),
        format!("label={LABEL_INSTANCE}={prefix}"),
    ];
    // **When this process started.** Reconciliation is spawned, not awaited
    // (`server::run`), so the router is already serving while it runs: by
    // the time `podman ps` answers, the first App-tab request may well have
    // started a service container of its own, and "every agent container whose
    // `lmgw.run` is not a live job" would happily collect it seconds after it
    // came up. A container younger than this process is by definition not a
    // leftover of a previous one (final review, §6.4).
    let born_epoch = born.timestamp();
    match registry.ps_filtered(&filters).await {
        Ok(rows) => {
            report.listed = true;
            for row in rows {
                // `>=`, and podman's second granularity means a container
                // created in the same second as the process start is kept.
                // Erring towards keeping is right: the cost is one leftover
                // collected on the next boot instead of this one, against
                // tearing down something that is in use.
                if row.created >= born_epoch {
                    continue;
                }
                let live = row
                    .labels
                    .get(LABEL_RUN)
                    .and_then(|r| r.parse::<i64>().ok())
                    .is_some_and(|id| state.jobs.live_one(id).is_some());
                if live {
                    continue;
                }
                match registry.rm_force(&row.name).await {
                    Ok(()) => {
                        tracing::info!(container = %row.name, "removed a leftover agent container");
                        report.removed.push(row.name);
                    }
                    Err(e) => report.errors.push(e),
                }
            }
        }
        Err(e) => report.errors.push(e),
    }
    sweep_run_dirs(state, born, &mut report);
    report
}

/// Every directory a run directory of this install could be sitting in.
///
/// Two roots, because [`runs_root`](super::runs_root) picks one **per boot**: with
/// `XDG_RUNTIME_DIR` set it is the tmpfs, without it the `<data_dir>/agents/`
/// fallback. An install that lost the variable for one boot (a `systemd`
/// service without `PAMName`, a console login) wrote that boot's `secrets.json`
/// to persistent storage, and sweeping only the root that is current now would
/// leave it there for good.
///
/// Under the **data dir** every prefix leaf is swept, not just the current
/// one: a `container_prefix` the owner has since changed leaves its whole
/// subtree orphaned, and a data directory belongs to exactly one install, so
/// there is nobody else's work in there.
///
/// Under **`$XDG_RUNTIME_DIR/lmgw/`** only this instance's leaf is swept, and
/// that is deliberate: that tree is shared by *every* lmgw on the box — which
/// is the entire reason [`runs_root`](super::runs_root) scopes itself by prefix — so walking a
/// sibling leaf would be one instance deleting another's live `secrets.json`,
/// the exact collision class the prefix exists to remove. Nothing is lost by
/// it: a tmpfs does not survive the session, so a stale prefix's leftovers
/// there die on their own.
fn sweep_roots(data_dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let leaf = slug(prefix);
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(x) = std::env::var("XDG_RUNTIME_DIR") {
        if !x.is_empty() {
            roots.push(PathBuf::from(x).join("lmgw").join(&leaf));
        }
    }
    let persistent = data_dir.join("agents");
    if let Ok(entries) = std::fs::read_dir(&persistent) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                roots.push(e.path());
            }
        }
    }
    let own = persistent.join(&leaf);
    if !roots.contains(&own) {
        roots.push(own);
    }
    roots.sort();
    roots.dedup();
    roots
}

/// The same rule against every run-directory root this install could have
/// written to ([`sweep_roots`]): a directory whose run is not live is a secrets
/// file nobody is reading.
fn sweep_run_dirs(
    state: &SharedState,
    born: chrono::DateTime<chrono::Utc>,
    report: &mut Reconciled,
) {
    let prefix = state.snapshot().settings.container_prefix.clone();
    let born = std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::new(
            born.timestamp().max(0) as u64,
            born.timestamp_subsec_nanos(),
        );
    for root in sweep_roots(&state.data_dir, &prefix) {
        sweep_one_root(state, &root, born, report);
    }
}

fn sweep_one_root(
    state: &SharedState,
    root: &Path,
    born: std::time::SystemTime,
    report: &mut Reconciled,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Younger than this process, so it belongs to a run *this* build
        // started while reconciliation was still on its way (the same reading
        // the container's `Created` gets above). Reconciliation is spawned, not
        // awaited, so this is a real race and not a theoretical one: the first
        // App-tab request can create `service-<id>/` before `podman ps`
        // answers.
        if entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|m| m >= born)
        {
            continue;
        }
        // A service directory (`service-<slug(id)>`, §6.5) has no job to be
        // live: its owner is a container this instance has not started yet, so
        // at boot it is a leftover by definition — the same reading the
        // `lmgw.run=service` label gets above. A package-read directory
        // (`pkg-lmgw-pkg-<rand>`, §3.4) is the same: it lives for three podman
        // invocations inside one op, so one that is still here at boot is the
        // wreckage of a read that was killed halfway.
        if !name.starts_with("service-")
            && !name.starts_with(crate::agents::package::RUN_DIR_PREFIX)
        {
            let Some(id) = name
                .strip_prefix("run-")
                .and_then(|n| n.parse::<i64>().ok())
            else {
                continue;
            };
            if state.jobs.live_one(id).is_some() {
                continue;
            }
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => report.swept_dirs.push(name),
            Err(e) => report
                .errors
                .push(format!("removing {}: {e}", entry.path().display())),
        }
    }
    // And the root itself when the sweep emptied it, the same best-effort,
    // non-recursive removal `RunDir`'s own `Drop` makes.
    let _ = std::fs::remove_dir(root);
}
