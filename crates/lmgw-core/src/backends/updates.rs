//! Update detection (container-builds design §8, §15 `build_updates_check`,
//! `UpdateStatus`, `BuildView.update`): whether a build's ref or its unpinned
//! extras moved since the run its image came from, and whether a registry
//! image in use has a newer digest upstream.
//!
//! # Observe, then evaluate
//!
//! A check **observes**: what each remote says its refs point at (one
//! `ls-remote` per distinct remote, asking only for the refs the builds
//! name), what the forge says about each unpinned PR (one call per PR), and
//! what each registry serves for each tag in use. Those facts are stored,
//! with the time they were fetched ([`UpdateStore`]).
//!
//! The badge is then **evaluated** from the stored facts against the build
//! as it is now and its reference run ([`store::reference_build_run`]: the
//! promoted run, else the newest succeeded or unverified one) — on every
//! read, with no network ([`evaluate`]). So a run that builds the new head
//! clears "master moved" the moment it finishes, and an edit shows
//! "definition changed since last run" the moment it is saved; neither waits
//! for the next check.
//!
//! # Light on the network (and the pool)
//!
//! Nothing is fetched into the git pool. A commit ref never moves and is not
//! asked about; a build with no comparable run costs nothing. "master +37
//! commits" needs both commits in the pool — when they are already there,
//! `rev-list --count` gives the number; otherwise the badge says "master
//! moved (abc1234 → def5678)" rather than fetching a history to count it.
//! A forge that rate-limits the check is asked no more for the rest of it,
//! and every build that needed it says why ("check failed: … resets at …").
//! The remotes are asked side by side, each given [`REMOTE_TIMEOUT`]: one
//! that stalls becomes "check failed: <remote> timed out after 60s" on the
//! builds that use it, never a check (or a **Check now**) that hangs.
//!
//! # Persistence
//!
//! The observations live on the gateway ([`UpdateStore`], reached through
//! [`BuildSeams::updates`](super::run::BuildSeams::updates)) and are written
//! to one `settings`-table row (`backends:update_check`) after every check —
//! by one writer at a time, each writing the state as it is when its turn
//! comes, so the row always ends as the newest state. A check of one build
//! and a full check running side by side merge per build: the newer
//! observation of a build is kept, whichever check finished last.
//! Restarts are frequent (lmgw is started by hand each morning; dev
//! instances far more often): with the last check's time persisted, the
//! schedule resumes where it was instead of re-asking every remote a few
//! minutes after each start, and the badge is right from the first frame.
//!
//! # Schedule
//!
//! [`run_scheduler`] runs [`check_all`] every `build_update_check_hours`
//! (`0` = off, at most [`MAX_CHECK_HOURS`], re-read every cycle), the first
//! time a few minutes after boot at the earliest. A check that fails is
//! logged once and tried again a [`TICK`] later, then twice that, doubling up
//! to the interval ([`retry_delay`]) — never a loop that asks again at once.
//! **Check now** is the `build_updates_check` op.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use lmgw_api_types::builds::{
    BuildUpdateEntry, BuildUpdatesResponse, ExtraChange, RegistryUpdate, UpdateStatus,
    UpdatesSummary,
};
use serde::{Deserialize, Serialize};

use super::forge;
use super::git::{self, remote_key, short_sha, Git, GitAuth, Pool};
use super::images::{configured_images, local_images, resolve, LocalImage};
use super::model::{Build, BuildExtra, BuildRun, BuildSpec, Forge};
use super::oci::{self, ImageRef, RemoteManifest};
use super::run::Podman;
use super::validate;
use crate::state::{AppState, SharedState};
use crate::store;

/// The `settings`-table row the observations are kept in.
pub const KV_KEY: &str = "backends:update_check";

/// The earliest a scheduled check runs after boot: the gateway settles first,
/// and a start that is quickly followed by a restart asks no remote.
pub const BOOT_DELAY: Duration = Duration::from_secs(3 * 60);

/// How often the scheduler wakes to re-read `build_update_check_hours` and
/// see whether a check is due. Waking costs nothing (no network, no git); it
/// is what makes a changed interval — or `0` — apply without a restart.
pub const TICK: Duration = Duration::from_secs(5 * 60);

/// How long one remote's `ls-remote` may take before the check gives up on
/// it: "check failed: <remote> timed out after 60s" on every build that uses
/// it (docs/backends.md §10). A narrow `ls-remote` answers in about half a
/// second; a minute is a remote that is not going to answer.
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest `build_update_check_hours` accepted: a year. A longer one is
/// refused where it is set, with this range in the message
/// ([`validate_check_hours`]). Shared with `lmgw-ui`'s Settings hint and
/// client-side check, so it is defined once in `lmgw-api-types`.
pub use lmgw_api_types::builds::MAX_BUILD_UPDATE_CHECK_HOURS as MAX_CHECK_HOURS;

/// `build_update_check_hours` as the two settings paths accept it: `0`
/// (off) to [`MAX_CHECK_HOURS`].
pub fn validate_check_hours(hours: u32) -> Result<u32, String> {
    if hours > MAX_CHECK_HOURS {
        return Err(format!(
            "build_update_check_hours {hours} is out of range — it must be between 0 (off) and \
             {MAX_CHECK_HOURS} (a year)"
        ));
    }
    Ok(hours)
}

/// `60s`, or `250ms` below a second (the tests' timeouts).
fn fmt_duration(d: Duration) -> String {
    if d >= Duration::from_secs(1) {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// Observations
// ---------------------------------------------------------------------------

/// What a remote said one ref points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Seen {
    Branch {
        sha: String,
    },
    /// Peeled: the commit an annotated tag names.
    Tag {
        commit: String,
    },
    /// A full ref that is neither a head nor a tag (`refs/pull/9/head`).
    Ref {
        sha: String,
    },
    /// Neither a branch nor a tag (nor the full ref) of that name exists.
    Missing,
    /// The remote could not be asked.
    Failed {
        error: String,
    },
}

impl Seen {
    /// The commit it names, when it names one.
    fn sha(&self) -> Option<&str> {
        match self {
            Self::Branch { sha } | Self::Ref { sha } | Self::Tag { commit: sha } => Some(sha),
            Self::Missing | Self::Failed { .. } => None,
        }
    }
}

/// One ref of one remote, as observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefSeen {
    /// [`remote_key`] of the URL, so two spellings of one remote match.
    pub remote: String,
    pub name: String,
    pub seen: Seen,
}

/// One PR/MR as the forge described it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrSeen {
    pub remote: String,
    pub number: u64,
    /// `open` | `closed` | `merged` (normalized by the forge client).
    pub state: String,
    pub head_sha: String,
    pub merged_at: Option<String>,
    /// The merge/squash/rebase commit the forge names for a merged PR
    /// (`ForgePr::merge_commit_sha`); empty when the forge gave none. Together
    /// with `head_sha`, what [`Contains`] is checked against — the same pair
    /// the executor's `merged_into` (`run/resolve.rs`) uses to decide whether
    /// a merged PR is already in a build's base.
    pub merge_commit_sha: String,
    /// Set when the forge could not be asked; the rest is then empty.
    pub error: Option<String>,
}

/// `to` is `count` commits ahead of `from` — known only when the pool held
/// both at check time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ahead {
    pub from: String,
    pub to: String,
    pub count: u64,
}

/// Whether `base`'s history contains `commit` (a merged PR's merge commit, or
/// failing that its head — [`PrSeen::merge_commit_sha`], `head_sha`) —
/// recorded only when the pool held both objects at check time
/// ([`git::Pool::contains`]). Its absence for a pair is not "no", it is
/// "unknown": the badge treats both the same, non-committally (§below,
/// [`evaluate`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contains {
    pub base: String,
    pub commit: String,
    pub result: bool,
}

/// Everything one check learned about one build's remotes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildObservation {
    pub checked_at: String,
    pub refs: Vec<RefSeen>,
    pub prs: Vec<PrSeen>,
    pub ahead: Vec<Ahead>,
    pub contains: Vec<Contains>,
}

impl BuildObservation {
    fn seen(&self, url: &str, name: &str) -> Option<&Seen> {
        let key = remote_key(url);
        self.refs
            .iter()
            .find(|r| r.remote == key && r.name == name)
            .map(|r| &r.seen)
    }

    fn pr(&self, url: &str, number: u64) -> Option<&PrSeen> {
        let key = remote_key(url);
        self.prs
            .iter()
            .find(|p| p.remote == key && p.number == number)
    }

    fn ahead(&self, from: &str, to: &str) -> Option<u64> {
        self.ahead
            .iter()
            .find(|a| a.from == from && a.to == to)
            .map(|a| a.count)
    }

    /// `None` when the pair was never checked (pool missing one or both
    /// objects at check time) — the caller treats that like `Some(false)`.
    fn contains(&self, base: &str, commit: &str) -> Option<bool> {
        self.contains
            .iter()
            .find(|c| c.base == base && c.commit == commit)
            .map(|c| c.result)
    }
}

/// What a registry said about one tag in use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageObservation {
    pub checked_at: String,
    /// Fully qualified (`ghcr.io/o/r:tag`).
    pub reference: String,
    pub remote_digest: Option<String>,
    /// The index's members, when it was read (see [`oci`]).
    pub members: Vec<String>,
    pub error: Option<String>,
    /// Evaluated against the local digests at check time, and again after a
    /// pull of this reference — what the nav badge's image count reads, so
    /// the live frame needs no podman call.
    pub update_available: bool,
}

impl ImageObservation {
    /// The wire shape against `digests`, the local image's `RepoDigests`.
    fn evaluate(&self, repo_digests: &[String]) -> RegistryUpdate {
        let repo = ImageRef::parse(&self.reference).map(|r| r.repo_name());
        let local = repo
            .as_deref()
            .map(|r| oci::local_digests(repo_digests, r))
            .unwrap_or_default();
        let mut out = RegistryUpdate {
            checked_at: self.checked_at.clone(),
            reference: self.reference.clone(),
            remote_digest: self.remote_digest.clone(),
            local_digests: local.clone(),
            update_available: false,
            error: self.error.clone(),
        };
        if out.error.is_some() {
            return out;
        }
        let Some(remote) = &self.remote_digest else {
            return out;
        };
        if local.is_empty() {
            out.error = Some(format!(
                "podman recorded no registry digest for {} on this image — it was built, loaded \
                 or tagged here rather than pulled, so there is nothing to compare the registry's \
                 {} with",
                repo.unwrap_or_else(|| self.reference.clone()),
                short_digest(remote)
            ));
            return out;
        }
        let served = RemoteManifest {
            digest: remote.clone(),
            members: self.members.clone(),
        };
        out.update_available = !served.matches(&local);
        out
    }
}

fn short_digest(d: &str) -> &str {
    let hex = d.strip_prefix("sha256:").unwrap_or(d);
    hex.get(..12).unwrap_or(hex)
}

/// The stored state, and its persisted form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stored {
    /// When the last full check ([`check_all`]) ended.
    pub last_all: Option<String>,
    pub builds: BTreeMap<i64, BuildObservation>,
    pub images: BTreeMap<String, ImageObservation>,
}

impl Stored {
    /// The registry status for a local image: the observation of one of its
    /// names (an outdated one first, when several of its names are in use).
    pub(crate) fn registry_update(&self, img: &LocalImage) -> Option<RegistryUpdate> {
        let mut found: Vec<RegistryUpdate> = img
            .names
            .iter()
            .filter_map(|n| self.images.get(n))
            .map(|o| o.evaluate(&img.digests))
            .collect();
        found.sort_by_key(|u| !u.update_available);
        found.into_iter().next()
    }

    pub fn last_all_time(&self) -> Option<DateTime<Utc>> {
        self.last_all.as_deref().and_then(parse_time)
    }
}

/// The gateway's update-check state: the stored observations (loaded from
/// the database on first use), the lock that keeps two full checks from
/// running at once, and the one that makes the database row's writers take
/// turns.
pub struct UpdateStore {
    stored: std::sync::Mutex<Stored>,
    loaded: tokio::sync::OnceCell<()>,
    check_lock: tokio::sync::Mutex<()>,
    persist_lock: tokio::sync::Mutex<()>,
    /// [`REMOTE_TIMEOUT`]; tests shorten it.
    remote_timeout: std::sync::Mutex<Duration>,
}

impl Default for UpdateStore {
    fn default() -> Self {
        Self {
            stored: std::sync::Mutex::default(),
            loaded: tokio::sync::OnceCell::new(),
            check_lock: tokio::sync::Mutex::new(()),
            persist_lock: tokio::sync::Mutex::new(()),
            remote_timeout: std::sync::Mutex::new(REMOTE_TIMEOUT),
        }
    }
}

impl UpdateStore {
    /// A copy of the stored state, loading it from the database the first
    /// time. An unreadable row is logged and treated as empty — the next
    /// check rewrites it.
    pub async fn loaded(&self, state: &AppState) -> Stored {
        self.loaded
            .get_or_init(|| async {
                match store::get_kv(&state.db, KV_KEY).await {
                    Ok(Some(json)) => match serde_json::from_str::<Stored>(&json) {
                        Ok(s) => *self.lock() = s,
                        Err(e) => tracing::warn!(
                            "the stored build update check is unreadable ({e}); it is rebuilt by \
                             the next check"
                        ),
                    },
                    Ok(None) => {}
                    Err(e) => tracing::warn!("reading the stored build update check: {e}"),
                }
            })
            .await;
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Stored> {
        self.stored.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// How long one remote may take to answer ([`REMOTE_TIMEOUT`]).
    pub fn remote_timeout(&self) -> Duration {
        *self
            .remote_timeout
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Shorten [`REMOTE_TIMEOUT`] — a test of a remote that never answers.
    pub fn set_remote_timeout_for_tests(&self, d: Duration) {
        *self
            .remote_timeout
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = d;
    }

    /// Write the state to the database. One writer at a time, and each
    /// serializes the state only once it is its turn — so whichever write
    /// lands last carries the newest state, never an older snapshot taken
    /// before another writer's change.
    async fn persist(&self, state: &AppState) {
        let _turn = self.persist_lock.lock().await;
        let json = match serde_json::to_string(&*self.lock()) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("serializing the build update check: {e}");
                return;
            }
        };
        if let Err(e) = store::set_kv(&state.db, KV_KEY, &json).await {
            tracing::warn!("saving the build update check: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluation (pure)
// ---------------------------------------------------------------------------

/// A ref no remote can move: a full commit SHA.
fn is_commit(name: &str) -> bool {
    git::is_full_sha(name)
}

/// A name that may be a short SHA (§14.1: resolved against branch and tag
/// tips at run time) — when no branch or tag of that name exists, it is a
/// commit, not a vanished ref.
fn maybe_short_sha(name: &str) -> bool {
    name.len() >= 4 && git::is_hex(name)
}

/// The same extra, whatever its pin: a PR by number, a ref by remote and name.
fn same_extra(a: &BuildExtra, b: &BuildExtra) -> bool {
    match (a, b) {
        (BuildExtra::Pr { number: x, .. }, BuildExtra::Pr { number: y, .. }) => x == y,
        (
            BuildExtra::Ref {
                remote_url: u1,
                git_ref: r1,
                ..
            },
            BuildExtra::Ref {
                remote_url: u2,
                git_ref: r2,
                ..
            },
        ) => r1 == r2 && remote_key(u1) == remote_key(u2),
        _ => false,
    }
}

/// The definition fields that change what a run builds, as named in the
/// badge (`definition changed since last run (ref, extras)`). Everything the
/// config hash covers (`tags::canonical_cfg`) plus the repository and ref —
/// the base SHA is in the tag, so a different branch is a different image.
/// Name, notes, `cpus`, `keep_layers` and `keep_runs` change how a run goes
/// or what it keeps, never the image, and are left out.
pub fn definition_changes(now: &BuildSpec, then: &BuildSpec) -> Vec<&'static str> {
    let mut out = Vec::new();
    if now.engine != then.engine {
        out.push("engine");
    }
    if remote_key(&now.repo_url) != remote_key(&then.repo_url) {
        out.push("repository");
    }
    if now.git_ref != then.git_ref {
        out.push("ref");
    }
    let extras = |s: &BuildSpec| -> Vec<(String, Option<String>)> {
        s.extras
            .iter()
            .map(|e| {
                let id = match e {
                    BuildExtra::Pr { number, .. } => format!("pr {number}"),
                    BuildExtra::Ref {
                        remote_url,
                        git_ref,
                        ..
                    } => format!("ref {} {git_ref}", remote_key(remote_url)),
                };
                (id, e.pin().map(str::to_ascii_lowercase))
            })
            .collect()
    };
    if extras(now) != extras(then) {
        out.push("extras");
    }
    if now.backend != then.backend {
        out.push("GPU backend");
    }
    if now.cuda_version != then.cuda_version {
        out.push("CUDA version");
    }
    if now.arch != then.arch {
        out.push("arch");
    }
    if now.dockerfile != then.dockerfile {
        out.push("Dockerfile");
    }
    if now.target != then.target {
        out.push("target");
    }
    let edits = |s: &BuildSpec| {
        s.edits.as_ref().map(|list| {
            list.iter()
                .map(|e| (e.find.clone(), e.replace.clone()))
                .collect::<Vec<_>>()
        })
    };
    if edits(now) != edits(then) {
        out.push("edits");
    }
    let args = |s: &BuildSpec| {
        validate::parse_build_args(&s.build_args)
            .unwrap_or_else(|_| vec![(String::new(), s.build_args.trim().to_string())])
    };
    if args(now) != args(then) {
        out.push("build args");
    }
    let ccache = |s: &BuildSpec| {
        (
            s.ccache,
            s.ccache.then(|| s.ccache_max_size.trim().to_string()),
        )
    };
    if ccache(now) != ccache(then) {
        out.push("ccache");
    }
    out
}

/// The badge of `build` from `obs`, against `run` — its reference run (§8).
/// Pure: every read re-evaluates, so a newer run or an edit shows at once.
pub fn evaluate(build: &Build, run: &BuildRun, obs: &BuildObservation) -> UpdateStatus {
    let spec = &build.spec;
    let then = &run.inputs.config;
    let resolved = run.inputs.resolved.as_ref();
    let base = resolved
        .map(|r| r.base_sha.as_str())
        .filter(|s| !s.is_empty())
        .or(run.base_sha.as_deref())
        .unwrap_or("");
    let mut st = UpdateStatus {
        checked_at: obs.checked_at.clone(),
        ..UpdateStatus::default()
    };
    let mut errors: Vec<String> = Vec::new();

    // The base ref — compared only while it is still the one the run built;
    // a different ref is a definition change, said below.
    let same_base =
        remote_key(&spec.repo_url) == remote_key(&then.repo_url) && spec.git_ref == then.git_ref;
    if same_base && !base.is_empty() && !is_commit(&spec.git_ref) {
        let name = &spec.git_ref;
        match obs.seen(&spec.repo_url, name) {
            Some(Seen::Tag { commit }) if commit != base => {
                st.ref_moved = true;
                st.reasons.push(format!(
                    "tag {name} moved ({} → {})",
                    short_sha(base),
                    short_sha(commit)
                ));
            }
            Some(Seen::Branch { sha } | Seen::Ref { sha }) if sha != base => {
                st.ref_moved = true;
                st.reasons.push(match obs.ahead(base, sha) {
                    Some(n) => format!("{name} +{n} commit{}", if n == 1 { "" } else { "s" }),
                    None => format!("{name} moved ({} → {})", short_sha(base), short_sha(sha)),
                });
            }
            Some(Seen::Missing) if !maybe_short_sha(name) => errors.push(format!(
                "check failed: {name} is no longer a branch or tag of {} — the next run cannot \
                 resolve it",
                spec.repo_url
            )),
            Some(Seen::Failed { error }) => errors.push(format!("check failed: {error}")),
            _ => {}
        }
    }

    for extra in spec.extras.iter().filter(|e| e.pin().is_none()) {
        let recorded = resolved
            .and_then(|r| r.extras.iter().find(|e| same_extra(&e.extra, extra)))
            .map(|e| e.sha.as_str());
        let label = extra.label();
        let change: Option<String> = match extra {
            BuildExtra::Pr { number, .. } => match obs.pr(&spec.repo_url, *number) {
                Some(PrSeen { error: Some(e), .. }) => {
                    errors.push(format!("check failed: {e}"));
                    None
                }
                Some(p) if p.merged_at.is_some() || p.state == "merged" => {
                    // The same pair the executor's `merged_into` decides a
                    // skip from (`run/resolve.rs`): the merge commit, else
                    // the head. `Some(true)` is the only case the base is
                    // known to already contain it; anything else (not an
                    // ancestor, or the pool never held both objects) reads
                    // the same, non-committal way.
                    let commit = [p.merge_commit_sha.as_str(), p.head_sha.as_str()]
                        .into_iter()
                        .find(|c| !c.is_empty());
                    let contained = commit.and_then(|c| obs.contains(base, c));
                    Some(if contained == Some(true) {
                        "merged upstream and contained in your base — drop it".into()
                    } else {
                        let when = p
                            .merged_at
                            .as_deref()
                            .map(|d| format!(" at {d}"))
                            .unwrap_or_default();
                        format!(
                            "merged upstream{when}; your base doesn't contain it yet — it's \
                             still merged into your build"
                        )
                    })
                }
                Some(p) if matches!(p.state.as_str(), "closed" | "locked") => {
                    Some("closed unmerged".into())
                }
                Some(p)
                    if recorded.is_some_and(|r| !p.head_sha.is_empty() && !p.head_sha.eq(r)) =>
                {
                    Some("pushed".into())
                }
                _ => None,
            },
            BuildExtra::Ref {
                remote_url,
                git_ref,
                ..
            } => match (recorded, obs.seen(remote_url, git_ref)) {
                _ if is_commit(git_ref) => None,
                (Some(r), Some(seen)) => match seen {
                    Seen::Failed { error } => {
                        errors.push(format!("check failed: {error}"));
                        None
                    }
                    Seen::Missing if !maybe_short_sha(git_ref) => {
                        errors.push(format!(
                            "check failed: {label}: {git_ref} no longer exists on {remote_url}"
                        ));
                        None
                    }
                    s => s
                        .sha()
                        .filter(|sha| *sha != r)
                        .map(|sha| format!("moved ({} → {})", short_sha(r), short_sha(sha))),
                },
                _ => None,
            },
        };
        if let Some(change) = change {
            st.reasons.push(format!("{label} {change}"));
            st.extras.push(ExtraChange { label, change });
        }
    }

    let changed = definition_changes(spec, then);
    if !changed.is_empty() {
        st.reasons.push(format!(
            "definition changed since last run ({})",
            changed.join(", ")
        ));
    }
    // One unreachable remote is one sentence, however many refs it holds.
    let mut seen = BTreeSet::new();
    errors.retain(|e| seen.insert(e.clone()));
    st.errors = errors;
    st
}

// ---------------------------------------------------------------------------
// Observation (network)
// ---------------------------------------------------------------------------

/// The full ref names to ask a remote for so that `name` resolves the way a
/// run resolves it (branch before tag, §14.1) — with the peeled `^{}` of a
/// tag, which `ls-remote` sends only when asked for by name.
fn patterns_for(name: &str) -> Vec<String> {
    if name.starts_with("refs/tags/") {
        vec![name.to_string(), format!("{name}^{{}}")]
    } else if name.starts_with("refs/") {
        vec![name.to_string()]
    } else {
        vec![
            format!("refs/heads/{name}"),
            format!("refs/tags/{name}"),
            format!("refs/tags/{name}^{{}}"),
        ]
    }
}

/// `name` in an `ls-remote` answer.
fn seen_in(answer: &HashMap<String, String>, name: &str) -> Seen {
    let tag = |t: &str| {
        answer
            .get(&format!("{t}^{{}}"))
            .or_else(|| answer.get(t))
            .map(|c| Seen::Tag { commit: c.clone() })
    };
    let hit = if let Some(b) = name.strip_prefix("refs/heads/") {
        answer
            .get(&format!("refs/heads/{b}"))
            .map(|sha| Seen::Branch { sha: sha.clone() })
    } else if name.starts_with("refs/tags/") {
        tag(name)
    } else if name.starts_with("refs/") {
        answer.get(name).map(|sha| Seen::Ref { sha: sha.clone() })
    } else {
        answer
            .get(&format!("refs/heads/{name}"))
            .map(|sha| Seen::Branch { sha: sha.clone() })
            .or_else(|| tag(&format!("refs/tags/{name}")))
    };
    hit.unwrap_or(Seen::Missing)
}

/// The refs a build's check asks about: `(url, forge for the token, name)`.
fn wanted_refs(state: &SharedState, spec: &BuildSpec) -> Vec<(String, Forge, String)> {
    let mut out = Vec::new();
    if !is_commit(&spec.git_ref) {
        out.push((spec.repo_url.clone(), spec.forge, spec.git_ref.clone()));
    }
    let snap = state.snapshot();
    let tokens = &snap.settings.forge_tokens;
    for e in &spec.extras {
        if let BuildExtra::Ref {
            remote_url,
            git_ref,
            pin: None,
        } = e
        {
            if !is_commit(git_ref) {
                // Whether the remote has a token, found the way the token
                // itself is ([`forge::token_host`]).
                let has_token =
                    forge::token_host(remote_url).is_some_and(|h| tokens.contains_key(&h));
                let forge = validate::default_forge(remote_url, |_| has_token);
                out.push((remote_url.clone(), forge, git_ref.clone()));
            }
        }
    }
    out
}

/// One remote to ask, with every ref any build wants from it. `auth` is the
/// refusal when the remote's token may not be sent to it (plain `http://`,
/// [`forge::git_auth`]) — the remote is then not asked, and says why.
struct Remote {
    url: String,
    auth: Result<Option<GitAuth>, String>,
    patterns: BTreeSet<String>,
}

/// `ask`, given at most `timeout` — a remote that does not answer in time is
/// an error that says so, and the `git` behind it is killed when `ask` is
/// dropped (`kill_on_drop`).
async fn within<T>(
    ask: impl std::future::Future<Output = Result<T, String>>,
    timeout: Duration,
    url: &str,
) -> Result<T, String> {
    tokio::time::timeout(timeout, ask)
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "{url} timed out after {} (git ls-remote)",
                fmt_duration(timeout)
            ))
        })
}

/// Observe the remotes of `targets` — builds paired with their reference
/// runs. One `ls-remote` per distinct remote, one forge call per distinct
/// unpinned PR, `rev-list --count` only for commits the pool already holds.
async fn observe(
    state: &SharedState,
    targets: &[(Build, BuildRun)],
) -> HashMap<i64, BuildObservation> {
    let checked_at = now_rfc3339();

    let tokens = state.snapshot().settings.forge_tokens.clone();
    let mut remotes: BTreeMap<String, Remote> = BTreeMap::new();
    for (b, _) in targets {
        for (url, forge, name) in wanted_refs(state, &b.spec) {
            let r = remotes.entry(remote_key(&url)).or_insert_with(|| Remote {
                auth: forge::git_auth(&tokens, &url, forge),
                url: url.clone(),
                patterns: BTreeSet::new(),
            });
            r.patterns.extend(patterns_for(&name));
        }
    }
    // Side by side, each within the timeout: one stalled remote costs the
    // check one timeout, not the rest of the remotes their turn.
    let git = Git::new();
    let timeout = state.builds.updates().remote_timeout();
    let asks = remotes.iter().map(|(key, r)| {
        let git = &git;
        async move {
            let patterns: Vec<String> = r.patterns.iter().cloned().collect();
            let answer = match &r.auth {
                Ok(auth) => {
                    within(
                        git.ls_remote_refs(&r.url, &patterns, auth.as_ref()),
                        timeout,
                        &r.url,
                    )
                    .await
                }
                Err(refused) => Err(format!("{}: {refused}", r.url)),
            };
            (key.clone(), answer)
        }
    });
    let answers: HashMap<String, Result<HashMap<String, String>, String>> =
        futures::future::join_all(asks).await.into_iter().collect();

    // One call per distinct PR; a forge that says its quota is gone is not
    // asked again during this check — every PR behind it gets its sentence.
    let forge = state.builds.forge();
    let mut prs: HashMap<(String, u64), PrSeen> = HashMap::new();
    let mut exhausted: HashMap<String, String> = HashMap::new();
    for (b, _) in targets {
        let spec = &b.spec;
        for e in &spec.extras {
            let BuildExtra::Pr { number, pin: None } = e else {
                continue;
            };
            let key = (remote_key(&spec.repo_url), *number);
            if prs.contains_key(&key) {
                continue;
            }
            let host = forge::token_host(&spec.repo_url).unwrap_or_default();
            let answer = match exhausted.get(&host) {
                Some(e) => Err(format!(
                    "PR #{number}: not asked, the forge's quota ran out earlier in this check \
                     ({e})"
                )),
                None => forge.pr(&spec.repo_url, spec.forge, *number).await,
            };
            let seen = match answer {
                Ok(p) => PrSeen {
                    remote: key.0.clone(),
                    number: *number,
                    state: p.state,
                    head_sha: p.head_sha,
                    merged_at: p.merged_at,
                    merge_commit_sha: p.merge_commit_sha,
                    error: None,
                },
                Err(e) => {
                    if e.contains("rate limit reached") && !exhausted.contains_key(&host) {
                        exhausted.insert(host.clone(), e.clone());
                    }
                    PrSeen {
                        remote: key.0.clone(),
                        number: *number,
                        error: Some(e),
                        ..PrSeen::default()
                    }
                }
            };
            prs.insert(key, seen);
        }
    }

    // The count behind "master +37 commits", only from what the pool has.
    let builds_dir = state.builds_dir();
    let pool = if Pool::exists(&builds_dir) {
        Pool::open(Git::new(), &builds_dir).await.ok()
    } else {
        None
    };

    let mut out = HashMap::new();
    for (b, run) in targets {
        let spec = &b.spec;
        let mut obs = BuildObservation {
            checked_at: checked_at.clone(),
            ..BuildObservation::default()
        };
        for (url, _, name) in wanted_refs(state, spec) {
            let key = remote_key(&url);
            let seen = match answers.get(&key) {
                Some(Ok(answer)) => seen_in(answer, &name),
                Some(Err(e)) => Seen::Failed { error: e.clone() },
                None => continue,
            };
            obs.refs.push(RefSeen {
                remote: key,
                name,
                seen,
            });
        }
        for e in &spec.extras {
            if let BuildExtra::Pr { number, pin: None } = e {
                if let Some(p) = prs.get(&(remote_key(&spec.repo_url), *number)) {
                    obs.prs.push(p.clone());
                }
            }
        }
        let base = run
            .inputs
            .resolved
            .as_ref()
            .map(|r| r.base_sha.clone())
            .or_else(|| run.base_sha.clone())
            .unwrap_or_default();
        if let (Some(pool), Some(Seen::Branch { sha } | Seen::Ref { sha })) =
            (&pool, obs.seen(&spec.repo_url, &spec.git_ref).cloned())
        {
            if !base.is_empty() && sha != base {
                match pool.ahead(&base, &sha).await {
                    Ok(Some(count)) => obs.ahead.push(Ahead {
                        from: base.clone(),
                        to: sha,
                        count,
                    }),
                    Ok(None) => {}
                    Err(e) => tracing::debug!("update check: counting {base}..{sha}: {e}"),
                }
            }
        }
        // Whether this base already contains a merged PR — the same
        // merge-commit-else-head pair the executor's `merged_into`
        // (`run/resolve.rs`) skips a PR on. Only when the pool has both
        // objects; otherwise the badge stays non-committal (`evaluate`).
        if let Some(pool) = &pool {
            if !base.is_empty() {
                for p in &obs.prs {
                    if p.merged_at.is_none() && p.state != "merged" {
                        continue;
                    }
                    let Some(commit) = [p.merge_commit_sha.as_str(), p.head_sha.as_str()]
                        .into_iter()
                        .find(|c| !c.is_empty())
                    else {
                        continue;
                    };
                    if obs.contains(&base, commit).is_some() {
                        continue;
                    }
                    match pool.contains(&base, commit).await {
                        Ok(result) => obs.contains.push(Contains {
                            base: base.clone(),
                            commit: commit.to_string(),
                            result,
                        }),
                        Err(e) => {
                            tracing::debug!("update check: whether {base} contains {commit}: {e}")
                        }
                    }
                }
            }
        }
        out.insert(b.id, obs);
    }
    out
}

/// The registry images in use (§8, last bullet): each class default and
/// model override that names a local image under a registry name, checked
/// once per reference. `None` when the local images could not be listed —
/// the previous observations then stay as they are.
async fn observe_images(state: &SharedState) -> Option<BTreeMap<String, ImageObservation>> {
    let images = match local_images(&Podman::of(state)).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("registry update check skipped: {e}");
            return None;
        }
    };
    let snap = state.snapshot();
    let mut targets: BTreeMap<String, (ImageRef, Vec<String>)> = BTreeMap::new();
    for c in configured_images(&snap) {
        // The local name the reference matched is the qualified one podman
        // pulled it under; an unqualified name that is a `localhost/` build
        // has no registry to ask.
        let Some((img, Some(name))) = resolve(&c.reference, &images) else {
            continue;
        };
        let Some(r) = ImageRef::parse(&name) else {
            continue;
        };
        targets
            .entry(name)
            .or_insert_with(|| (r, img.digests.clone()));
    }
    let checked_at = now_rfc3339();
    let client = state.builds.registry_client();
    let mut out = BTreeMap::new();
    for (name, (r, repo_digests)) in targets {
        let local = oci::local_digests(&repo_digests, &r.repo_name());
        let answer = match &client {
            Ok(c) => c.remote_manifest(&r, &local).await,
            Err(e) => Err(e.clone()),
        };
        let mut obs = match answer {
            Ok(m) => ImageObservation {
                checked_at: checked_at.clone(),
                reference: name.clone(),
                remote_digest: Some(m.digest),
                members: m.members,
                ..ImageObservation::default()
            },
            Err(e) => ImageObservation {
                checked_at: checked_at.clone(),
                reference: name.clone(),
                error: Some(e),
                ..ImageObservation::default()
            },
        };
        obs.update_available = obs.evaluate(&repo_digests).update_available;
        out.insert(name, obs);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/// Whether `a` was observed strictly later than `b` (an unreadable time is
/// never later).
fn observed_later(a: &BuildObservation, b: &BuildObservation) -> bool {
    match (parse_time(&a.checked_at), parse_time(&b.checked_at)) {
        (Some(x), Some(y)) => x > y,
        _ => false,
    }
}

/// Store `obs` as `build_id`'s observation — unless the one stored is newer
/// (a check of that build that began later and finished first).
fn store_build(s: &mut Stored, build_id: i64, obs: BuildObservation) {
    match s.builds.get(&build_id) {
        Some(cur) if observed_later(cur, &obs) => {}
        _ => {
            s.builds.insert(build_id, obs);
        }
    }
}

/// A full check's results into the stored state: each build's observation
/// per [`store_build`]; a build the check found nothing to compare for
/// (deleted, or without a comparable run) drops out — unless it was checked
/// on its own since this check began (`began`, whole seconds like every
/// `checked_at`), which is newer than anything this check knows about it.
fn merge_full_check(
    s: &mut Stored,
    checked: &BTreeSet<i64>,
    observed: HashMap<i64, BuildObservation>,
    began: DateTime<Utc>,
) {
    s.builds.retain(|id, cur| {
        checked.contains(id) || parse_time(&cur.checked_at).is_some_and(|t| t >= began)
    });
    for (id, obs) in observed {
        store_build(s, id, obs);
    }
}

/// The run `build_id`'s badge compares against, if any.
async fn reference_run(state: &AppState, build_id: i64) -> Result<Option<BuildRun>, String> {
    store::reference_build_run(&state.db, build_id)
        .await
        .map_err(|e| e.to_string())
}

/// The badge of `build` right now, from the stored observations: `None`
/// before its first check, and for a build with no comparable run.
pub async fn status_for(state: &AppState, build: &Build) -> Option<UpdateStatus> {
    let stored = state.builds.updates().loaded(state).await;
    let obs = stored.builds.get(&build.id)?;
    let run = reference_run(state, build.id).await.ok()??;
    Some(evaluate(build, &run, obs))
}

/// Check one build now (**Check now** on its row): observe its remotes,
/// store what was seen, and return its badge. `Ok(None)` when it has no
/// succeeded or unverified run to compare with — nothing is asked then.
pub async fn check_build(
    state: &SharedState,
    build_id: i64,
) -> Result<Option<UpdateStatus>, String> {
    let build = store::get_build(&state.db, build_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no build with id {build_id}"))?;
    let updates = state.builds.updates();
    updates.loaded(state).await;
    let Some(run) = reference_run(state, build_id).await? else {
        let had = updates.lock().builds.remove(&build_id).is_some();
        if had {
            updates.persist(state).await;
        }
        return Ok(None);
    };
    let targets = [(build, run)];
    let obs = observe(state, &targets)
        .await
        .remove(&build_id)
        .unwrap_or_default();
    let [(build, run)] = targets;
    let status = evaluate(&build, &run, &obs);
    store_build(&mut updates.lock(), build_id, obs);
    updates.persist(state).await;
    publish(state).await;
    Ok(Some(status))
}

/// Check every build and every registry image in use (the schedule, and
/// **Check now** on the page). A call that had to wait for a check already
/// running answers with that check's results instead of asking every remote
/// a second time.
pub async fn check_all(state: &SharedState) -> Result<BuildUpdatesResponse, String> {
    let updates = state.builds.updates();
    updates.loaded(state).await;
    let asked = Utc::now();
    let _running = updates.check_lock.lock().await;
    let ended_meanwhile = updates
        .lock()
        .last_all
        .as_deref()
        .and_then(parse_time)
        .is_some_and(|t| t >= asked);
    if ended_meanwhile {
        return respond(state).await;
    }

    let builds = store::list_builds(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let began = parse_time(&now_rfc3339()).unwrap_or_else(Utc::now);
    let mut targets = Vec::new();
    for b in builds {
        if let Some(run) = reference_run(state, b.id).await? {
            targets.push((b, run));
        }
    }
    let observed = observe(state, &targets).await;
    let images = observe_images(state).await;
    {
        let mut s = updates.lock();
        // A build that was deleted, or lost its last comparable run, drops
        // out rather than keeping a badge nothing can clear; one checked on
        // its own meanwhile keeps that newer observation.
        let checked: BTreeSet<i64> = targets.iter().map(|(b, _)| b.id).collect();
        merge_full_check(&mut s, &checked, observed, began);
        if let Some(images) = images {
            s.images = images;
        }
        s.last_all = Some(now_rfc3339());
    }
    updates.persist(state).await;
    publish(state).await;
    respond(state).await
}

/// Every build's badge as stored (no network).
async fn respond(state: &SharedState) -> Result<BuildUpdatesResponse, String> {
    let builds = store::list_builds(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let mut updates = Vec::with_capacity(builds.len());
    for b in &builds {
        updates.push(BuildUpdateEntry {
            build_id: b.id,
            update: status_for(state, b).await,
        });
    }
    Ok(BuildUpdatesResponse { updates })
}

/// After `podman pull <reference>`: re-evaluate its stored observation
/// against the digests the pull left (`repo_digests`, podman's
/// `RepoDigests`), so the badge drops without asking the registry again.
///
/// It waits for a full check in progress: that check's registry answers were
/// evaluated against the digests from before the pull, and re-evaluating
/// only after it has stored them is what keeps its older answer from
/// overwriting this one.
pub async fn after_pull(state: &SharedState, reference: &str, repo_digests: &[String]) {
    let updates = state.builds.updates();
    updates.loaded(state).await;
    let _after_any_check = updates.check_lock.lock().await;
    let changed = {
        let mut s = updates.lock();
        match s.images.get_mut(reference) {
            Some(obs) => {
                obs.update_available = obs.evaluate(repo_digests).update_available;
                true
            }
            None => false,
        }
    };
    if changed {
        updates.persist(state).await;
    }
    publish(state).await;
}

/// The nav badge's numbers (§8): builds with an update, registry images in
/// use with a newer digest, and when the last full check ended. Database
/// reads only.
pub async fn summary(state: &AppState) -> UpdatesSummary {
    let stored = state.builds.updates().loaded(state).await;
    let mut builds_with_updates = 0;
    if let Ok(builds) = store::list_builds(&state.db).await {
        for b in builds.iter().filter(|b| stored.builds.contains_key(&b.id)) {
            if status_for(state, b).await.is_some_and(|u| u.has_update()) {
                builds_with_updates += 1;
            }
        }
    }
    UpdatesSummary {
        builds_with_updates,
        images_with_updates: stored
            .images
            .values()
            .filter(|o| o.update_available)
            .count() as u32,
        checked_at: stored.last_all,
    }
}

/// Send the current [`summary`] to every open dashboard (the `updates` frame
/// of `/api/events`). Called whenever it can have changed: a check ended, a
/// run finished, a build was saved, promoted or verified, an image pulled.
pub async fn publish(state: &AppState) {
    state.telemetry.updates(summary(state).await);
}

// ---------------------------------------------------------------------------
// The schedule
// ---------------------------------------------------------------------------

/// When the scheduler wakes (tests shorten both).
#[derive(Debug, Clone, Copy)]
pub struct Cadence {
    pub boot_delay: Duration,
    pub tick: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            boot_delay: BOOT_DELAY,
            tick: TICK,
        }
    }
}

/// When the next full check is due: `None` while `hours` is `0` (off), and
/// for an interval that reaches past the last date chrono can hold (a stored
/// value from before [`MAX_CHECK_HOURS`] was enforced) — never a panic; at
/// once when none has run yet; else `hours` after the last.
pub fn due_at(
    hours: u32,
    last: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if hours == 0 {
        return None;
    }
    match last {
        None => Some(now),
        Some(l) => l.checked_add_signed(chrono::Duration::try_hours(i64::from(hours))?),
    }
}

/// Whether a full check is due: never while `hours` is `0` (off), at once
/// when none has run yet, else once `hours` have passed since the last.
pub fn is_due(hours: u32, last: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    due_at(hours, last, now).is_some_and(|due| now >= due)
}

/// How long the scheduler waits after the `failures`-th failed check in a
/// row before it tries again: a tick, doubling with each failure, never
/// longer than the interval — and never less than a tick, so a check that
/// keeps failing is tried a handful of times an hour, not in a loop.
pub fn retry_delay(failures: u32, tick: Duration, hours: u32) -> Duration {
    let interval = Duration::from_secs(u64::from(hours).saturating_mul(3600));
    let doubling = 1u32
        .checked_shl(failures.saturating_sub(1))
        .unwrap_or(u32::MAX);
    tick.saturating_mul(doubling).min(interval).max(tick)
}

/// How long the scheduler sleeps: until the next check is due — or, after a
/// failure, until its retry time (`retry_at`), whichever is later — but at
/// most a tick, so a changed interval applies within one.
pub fn next_wait(
    hours: u32,
    last: Option<DateTime<Utc>>,
    retry_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    tick: Duration,
) -> Duration {
    let Some(due) = due_at(hours, last, now) else {
        return tick;
    };
    let due = retry_at.map_or(due, |r| due.max(r));
    (due - now).to_std().unwrap_or(Duration::ZERO).min(tick)
}

/// Run [`check_all`] every `build_update_check_hours` for as long as the
/// gateway runs — the first time `cadence.boot_delay` after start at the
/// earliest, and not before the persisted last check is that old. The
/// setting is re-read every cycle, so `0` stops it and a new interval applies
/// without a restart. A failed check is logged once and retried after
/// [`retry_delay`]; a panicking one is caught (each check runs as its own
/// task), never the end of the loop.
pub async fn run_scheduler(state: SharedState, cadence: Cadence) {
    schedule(state, cadence, |st| async move { check_all(&st).await }).await
}

/// [`run_scheduler`] with the check itself passed in — a test's failing one.
async fn schedule<F, Fut>(state: SharedState, cadence: Cadence, check: F)
where
    F: Fn(SharedState) -> Fut,
    Fut: std::future::Future<Output = Result<BuildUpdatesResponse, String>> + Send + 'static,
{
    tokio::time::sleep(cadence.boot_delay).await;
    // Failed checks in a row, and when the next attempt may be made.
    let mut retry: Option<(u32, DateTime<Utc>)> = None;
    loop {
        let hours = state.snapshot().settings.build_update_check_hours;
        let last = state.builds.updates().loaded(&state).await.last_all_time();
        let now = Utc::now();
        let held_back = retry.is_some_and(|(_, at)| now < at);
        if is_due(hours, last, now) && !held_back {
            let failed = match tokio::spawn(check(state.clone())).await {
                Ok(Ok(r)) => {
                    let n = r
                        .updates
                        .iter()
                        .filter(|u| u.update.as_ref().is_some_and(UpdateStatus::has_update))
                        .count();
                    tracing::info!(
                        "build update check: {n} of {} build(s) have updates",
                        r.updates.len()
                    );
                    None
                }
                Ok(Err(e)) => Some(format!("failed: {e}")),
                Err(e) => Some(format!("panicked: {e}")),
            };
            retry = failed.map(|why| {
                let failures = retry.map_or(1, |(n, _)| n.saturating_add(1));
                let delay = retry_delay(failures, cadence.tick, hours);
                let at = chrono::Duration::from_std(delay)
                    .ok()
                    .and_then(|d| Utc::now().checked_add_signed(d))
                    .unwrap_or(DateTime::<Utc>::MAX_UTC);
                tracing::warn!(
                    "build update check {why} — {failures} failure(s) in a row; the next attempt \
                     is in {}",
                    fmt_duration(delay)
                );
                (failures, at)
            });
        }
        let last = state.builds.updates().loaded(&state).await.last_all_time();
        let wait = next_wait(
            hours,
            last,
            retry.map(|(_, at)| at),
            Utc::now(),
            cadence.tick,
        );
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::builds::{
        BuildEdit, BuildRunInputs, BuildRunStatus, ResolvedExtra, ResolvedInputs,
    };

    fn sha(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn spec() -> BuildSpec {
        BuildSpec {
            slug: "official-master".into(),
            repo_url: "https://github.com/ggml-org/llama.cpp".into(),
            forge: Forge::Github,
            git_ref: "master".into(),
            extras: vec![
                BuildExtra::Pr {
                    number: 7,
                    pin: None,
                },
                BuildExtra::Ref {
                    remote_url: "https://github.com/fork/llama.cpp".into(),
                    git_ref: "feature".into(),
                    pin: None,
                },
            ],
            ..BuildSpec::default()
        }
    }

    fn run_of(spec: &BuildSpec, base: &str, extras: &[(usize, char)]) -> BuildRun {
        BuildRun {
            id: 1,
            build_id: Some(1),
            status: BuildRunStatus::Succeeded,
            promoted: true,
            base_sha: Some(base.into()),
            inputs: BuildRunInputs {
                config: spec.clone(),
                resolved: Some(ResolvedInputs {
                    base_sha: base.into(),
                    extras: extras
                        .iter()
                        .map(|(i, c)| ResolvedExtra {
                            extra: spec.extras[*i].clone(),
                            sha: sha(*c),
                        })
                        .collect(),
                    ..ResolvedInputs::default()
                }),
                cfg_hash: None,
            },
            ..BuildRun::default()
        }
    }

    fn build(spec: BuildSpec) -> Build {
        Build {
            id: 1,
            spec,
            ..Build::default()
        }
    }

    fn seen(url: &str, name: &str, s: Seen) -> RefSeen {
        RefSeen {
            remote: remote_key(url),
            name: name.into(),
            seen: s,
        }
    }

    fn pr(state: &str, head: char, merged: bool) -> PrSeen {
        PrSeen {
            remote: remote_key("https://github.com/ggml-org/llama.cpp"),
            number: 7,
            state: state.into(),
            head_sha: sha(head),
            merged_at: merged.then(|| "2026-09-25T00:00:00Z".into()),
            merge_commit_sha: String::new(),
            error: None,
        }
    }

    /// A merged PR, with the forge's merge commit set (or not, `merge`
    /// `None`) — for the "contained in the base or not" badge wording.
    fn merged_pr(head: char, merge: Option<char>) -> PrSeen {
        PrSeen {
            merge_commit_sha: merge.map(sha).unwrap_or_default(),
            ..pr("merged", head, true)
        }
    }

    #[test]
    fn nothing_moved_is_up_to_date() {
        let s = spec();
        let run = run_of(&s, &sha('a'), &[(0, 'b'), (1, 'c')]);
        let obs = BuildObservation {
            checked_at: "t".into(),
            refs: vec![
                seen(&s.repo_url, "master", Seen::Branch { sha: sha('a') }),
                seen(
                    "https://github.com/fork/llama.cpp",
                    "feature",
                    Seen::Branch { sha: sha('c') },
                ),
            ],
            prs: vec![pr("open", 'b', false)],
            ahead: vec![],
            contains: vec![],
        };
        let u = evaluate(&build(s), &run, &obs);
        assert!(!u.has_update(), "{u:?}");
        assert!(u.errors.is_empty());
        assert_eq!(u.checked_at, "t");
    }

    #[test]
    fn moved_refs_pushed_prs_and_counts_are_worded_as_the_spec_says() {
        let s = spec();
        let run = run_of(&s, &sha('a'), &[(0, 'b'), (1, 'c')]);
        let mut obs = BuildObservation {
            checked_at: "t".into(),
            refs: vec![
                seen(&s.repo_url, "master", Seen::Branch { sha: sha('d') }),
                seen(
                    // Another spelling of the same remote.
                    "git@github.com:fork/llama.cpp.git",
                    "feature",
                    Seen::Branch { sha: sha('e') },
                ),
            ],
            prs: vec![pr("open", 'f', false)],
            ahead: vec![],
            contains: vec![],
        };
        let u = evaluate(&build(s.clone()), &run, &obs);
        assert!(u.ref_moved);
        assert_eq!(
            u.reasons,
            vec![
                "master moved (aaaaaaa → ddddddd)".to_string(),
                "PR #7 pushed".to_string(),
                "https://github.com/fork/llama.cpp feature moved (ccccccc → eeeeeee)".to_string(),
            ]
        );
        assert_eq!(u.extras[0].change, "pushed");
        obs.ahead.push(Ahead {
            from: sha('a'),
            to: sha('d'),
            count: 37,
        });
        assert_eq!(
            evaluate(&build(s), &run, &obs).reasons[0],
            "master +37 commits"
        );
    }

    #[test]
    fn merged_and_closed_prs_win_over_pushed_and_show_even_when_skipped() {
        let s = spec();
        // The run skipped PR #7 (merged upstream at the time): no recorded SHA.
        let run = run_of(&s, &sha('a'), &[(1, 'c')]);
        let mut obs = BuildObservation {
            checked_at: "t".into(),
            prs: vec![pr("merged", 'f', true)],
            ..BuildObservation::default()
        };
        let u = evaluate(&build(s.clone()), &run, &obs);
        assert_eq!(
            u.reasons,
            vec![
                "PR #7 merged upstream at 2026-09-25T00:00:00Z; your base doesn't contain it \
                 yet — it's still merged into your build"
            ],
            "the pool never held the base or the PR's commits: non-committal"
        );
        obs.prs = vec![pr("closed", 'f', false)];
        let u = evaluate(&build(s.clone()), &run, &obs);
        assert_eq!(u.reasons, vec!["PR #7 closed unmerged"]);
        // Open and moved, but the run never merged it: not "pushed".
        obs.prs = vec![pr("open", 'f', false)];
        assert!(!evaluate(&build(s), &run, &obs).has_update());
    }

    /// The badge matches the executor's `merged_into` (`run/resolve.rs`)
    /// exactly: "drop it" only once the pool has proven the base already
    /// contains the merge (or head) commit; a confirmed miss and an unproven
    /// pair (the pool lacking one or both objects) read the same,
    /// non-committal way.
    #[test]
    fn a_merged_pr_is_drop_it_only_once_the_pool_proves_the_base_contains_it() {
        let s = spec();
        let run = run_of(&s, &sha('a'), &[(1, 'c')]);
        let noncommittal = "PR #7 merged upstream at 2026-09-25T00:00:00Z; your base doesn't \
                             contain it yet — it's still merged into your build";

        // The merge commit is in the base: drop it.
        let obs = BuildObservation {
            checked_at: "t".into(),
            prs: vec![merged_pr('f', Some('m'))],
            contains: vec![Contains {
                base: sha('a'),
                commit: sha('m'),
                result: true,
            }],
            ..BuildObservation::default()
        };
        assert_eq!(
            evaluate(&build(s.clone()), &run, &obs).reasons,
            vec!["PR #7 merged upstream and contained in your base — drop it"]
        );

        // The pool has both objects and proved the base does *not* contain
        // the merge commit: non-committal, not "drop it".
        let obs = BuildObservation {
            checked_at: "t".into(),
            prs: vec![merged_pr('f', Some('m'))],
            contains: vec![Contains {
                base: sha('a'),
                commit: sha('m'),
                result: false,
            }],
            ..BuildObservation::default()
        };
        assert_eq!(
            evaluate(&build(s.clone()), &run, &obs).reasons,
            vec![noncommittal]
        );

        // No merge commit from the forge: the head is checked instead, and
        // the base contains it.
        let obs = BuildObservation {
            checked_at: "t".into(),
            prs: vec![merged_pr('f', None)],
            contains: vec![Contains {
                base: sha('a'),
                commit: sha('f'),
                result: true,
            }],
            ..BuildObservation::default()
        };
        assert_eq!(
            evaluate(&build(s.clone()), &run, &obs).reasons,
            vec!["PR #7 merged upstream and contained in your base — drop it"]
        );

        // The pool never held the objects (no `Contains` entry at all):
        // unknown reads exactly like a confirmed miss.
        let obs = BuildObservation {
            checked_at: "t".into(),
            prs: vec![merged_pr('f', Some('m'))],
            ..BuildObservation::default()
        };
        assert_eq!(evaluate(&build(s), &run, &obs).reasons, vec![noncommittal]);
    }

    #[test]
    fn pins_commits_and_tags() {
        let mut s = spec();
        s.git_ref = sha('a');
        s.extras = vec![BuildExtra::Pr {
            number: 7,
            pin: Some(sha('b')),
        }];
        let run = run_of(&s, &sha('a'), &[(0, 'b')]);
        let obs = BuildObservation {
            checked_at: "t".into(),
            refs: vec![seen(&s.repo_url, &sha('a'), Seen::Branch { sha: sha('z') })],
            prs: vec![pr("open", 'f', false)],
            ..BuildObservation::default()
        };
        assert!(
            !evaluate(&build(s), &run, &obs).has_update(),
            "a commit ref and a pinned PR never update"
        );

        let mut s = spec();
        s.git_ref = "b7000".into();
        s.extras.clear();
        let run = run_of(&s, &sha('a'), &[]);
        let obs = BuildObservation {
            checked_at: "t".into(),
            refs: vec![seen(&s.repo_url, "b7000", Seen::Tag { commit: sha('d') })],
            ..BuildObservation::default()
        };
        let u = evaluate(&build(s), &run, &obs);
        assert_eq!(u.reasons, vec!["tag b7000 moved (aaaaaaa → ddddddd)"]);
        assert!(u.ref_moved);
    }

    #[test]
    fn failures_are_errors_not_reasons() {
        let s = spec();
        let run = run_of(&s, &sha('a'), &[(0, 'b'), (1, 'c')]);
        let mut rate = pr("", 'b', false);
        rate.head_sha.clear();
        rate.error = Some(
            "PR #7 of ggml-org/llama.cpp: GitHub API rate limit reached — it resets at 14:32 (in \
             23 min)"
                .into(),
        );
        let obs = BuildObservation {
            checked_at: "t".into(),
            refs: vec![
                seen(
                    &s.repo_url,
                    "master",
                    Seen::Failed {
                        error: "git ls-remote x failed".into(),
                    },
                ),
                seen(
                    "https://github.com/fork/llama.cpp",
                    "feature",
                    Seen::Missing,
                ),
            ],
            prs: vec![rate],
            ..BuildObservation::default()
        };
        let u = evaluate(&build(s), &run, &obs);
        assert!(!u.has_update());
        assert_eq!(u.errors.len(), 3, "{:?}", u.errors);
        assert!(u.errors.iter().all(|e| e.starts_with("check failed: ")));
        assert!(u.errors[1].contains("rate limit reached") && u.errors[1].contains("resets at"));
        assert!(u.errors[2].contains("no longer exists"));
    }

    #[test]
    fn a_definition_change_names_its_fields_and_a_new_ref_is_not_moved() {
        let s = spec();
        let run = run_of(&s, &sha('a'), &[(0, 'b'), (1, 'c')]);
        let mut now = s.clone();
        now.git_ref = "release".into();
        now.ccache_max_size = "20G".into();
        now.edits = Some(vec![BuildEdit::default()]);
        now.notes = "not an image change".into();
        now.keep_runs = Some(1);
        let obs = BuildObservation {
            checked_at: "t".into(),
            refs: vec![seen(&s.repo_url, "release", Seen::Branch { sha: sha('d') })],
            prs: vec![pr("open", 'b', false)],
            ..BuildObservation::default()
        };
        let u = evaluate(&build(now), &run, &obs);
        assert!(!u.ref_moved, "the run built another ref");
        assert_eq!(
            u.reasons,
            vec!["definition changed since last run (ref, edits, ccache)"]
        );
        let mut pinned = s.clone();
        pinned.extras[0] = BuildExtra::Pr {
            number: 7,
            pin: Some(sha('b')),
        };
        assert_eq!(definition_changes(&pinned, &s), vec!["extras"]);
        let mut respelled = s.clone();
        respelled.repo_url = "git@github.com:ggml-org/llama.cpp.git".into();
        assert!(definition_changes(&respelled, &s).is_empty());
    }

    #[test]
    fn ls_remote_answers_resolve_branch_before_tag_and_peel_tags() {
        let mut a = HashMap::new();
        a.insert("refs/heads/x".to_string(), sha('1'));
        a.insert("refs/tags/x".to_string(), sha('2'));
        a.insert("refs/tags/v1".to_string(), sha('3'));
        a.insert("refs/tags/v1^{}".to_string(), sha('4'));
        a.insert("refs/pull/9/head".to_string(), sha('5'));
        assert_eq!(seen_in(&a, "x"), Seen::Branch { sha: sha('1') });
        assert_eq!(seen_in(&a, "v1"), Seen::Tag { commit: sha('4') });
        assert_eq!(seen_in(&a, "refs/tags/x"), Seen::Tag { commit: sha('2') });
        assert_eq!(seen_in(&a, "refs/pull/9/head"), Seen::Ref { sha: sha('5') });
        assert_eq!(seen_in(&a, "refs/heads/nope"), Seen::Missing);
        assert_eq!(
            patterns_for("master"),
            vec![
                "refs/heads/master",
                "refs/tags/master",
                "refs/tags/master^{}"
            ]
        );
        assert_eq!(patterns_for("refs/pull/9/head"), vec!["refs/pull/9/head"]);
    }

    #[test]
    fn the_schedule_honours_zero_and_the_interval() {
        let now = Utc::now();
        assert!(!is_due(0, None, now), "0 is off");
        assert!(!is_due(0, Some(now - chrono::Duration::days(9)), now));
        assert!(is_due(6, None, now), "never checked: due");
        assert!(!is_due(6, Some(now - chrono::Duration::hours(5)), now));
        assert!(is_due(6, Some(now - chrono::Duration::hours(6)), now));
    }

    #[test]
    fn an_interval_past_the_last_date_never_panics_and_never_comes_due() {
        let now = Utc::now();
        let tick = Duration::from_secs(300);
        // What a row saved before the range was enforced can still hold.
        assert!(!is_due(u32::MAX, Some(now), now));
        assert_eq!(due_at(u32::MAX, Some(now), now), None);
        assert_eq!(next_wait(u32::MAX, Some(now), None, now, tick), tick);
        assert!(is_due(u32::MAX, None, now), "never checked: due at once");
        assert!(retry_delay(40, tick, u32::MAX) >= tick);
    }

    #[test]
    fn the_interval_is_zero_to_a_year_and_says_so() {
        assert_eq!(validate_check_hours(0), Ok(0));
        assert_eq!(validate_check_hours(MAX_CHECK_HOURS), Ok(8760));
        let e = validate_check_hours(8761).unwrap_err();
        assert!(e.contains("between 0 (off) and 8760"), "{e}");
    }

    /// The respin: a failed check left `last_all` where it was, the next due
    /// time stayed in the past, and the scheduler slept 1 ms and asked again.
    #[test]
    fn after_a_failed_check_the_scheduler_waits_at_least_a_tick() {
        let now = Utc::now();
        let tick = Duration::from_secs(300);
        let hour = chrono::Duration::hours(1);
        assert_eq!(retry_delay(1, tick, 6), tick);
        assert_eq!(retry_delay(2, tick, 6), tick * 2);
        assert_eq!(retry_delay(3, tick, 6), tick * 4);
        assert_eq!(retry_delay(30, tick, 6), Duration::from_secs(6 * 3600));
        assert_eq!(retry_delay(9, tick, 1), Duration::from_secs(3600), "capped");
        assert_eq!(
            retry_delay(1, Duration::from_secs(7200), 1),
            Duration::from_secs(7200),
            "never under a tick"
        );
        // Never checked, or last checked long ago: due now, but the failed
        // attempt's retry time holds the next one back.
        let retry_at = now + chrono::Duration::from_std(tick).unwrap();
        for last in [None, Some(now - hour * 24)] {
            assert_eq!(next_wait(6, last, None, now, tick), Duration::ZERO);
            assert_eq!(next_wait(6, last, Some(retry_at), now, tick), tick);
        }
        // A retry time before the next due time changes nothing.
        let last = now - hour;
        assert_eq!(
            next_wait(6, Some(last), Some(retry_at), now, tick),
            tick,
            "woken each tick to see a changed interval"
        );
    }

    #[tokio::test]
    async fn a_failing_check_is_retried_with_backoff_not_in_a_loop() {
        let state = AppState::init_for_tests().await.unwrap();
        let mut s = state.snapshot().settings.clone();
        s.build_update_check_hours = 6;
        store::save_settings(&state.db, &s).await.unwrap();
        state.reload_snapshot().await.unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        let task = tokio::spawn(schedule(
            state.clone(),
            Cadence {
                boot_delay: Duration::ZERO,
                tick: Duration::from_millis(40),
            },
            move |_| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err("the database is locked".to_string()) }
            },
        ));
        tokio::time::sleep(Duration::from_millis(400)).await;
        task.abort();
        let n = calls.load(std::sync::atomic::Ordering::SeqCst);
        // Attempts at 0, 40, 120, 280 ms (a tick, doubling): a handful, where
        // the 1 ms respin made hundreds.
        assert!((2..=6).contains(&n), "{n} attempts in 400 ms");
    }

    #[tokio::test]
    async fn a_remote_that_never_answers_times_out_with_a_sentence() {
        let e = within(
            std::future::pending::<Result<(), String>>(),
            Duration::from_millis(20),
            "https://git.example/g/p",
        )
        .await
        .unwrap_err();
        assert_eq!(
            e,
            "https://git.example/g/p timed out after 20ms (git ls-remote)"
        );
        assert_eq!(fmt_duration(REMOTE_TIMEOUT), "60s");
        assert_eq!(
            within(async { Ok(7) }, Duration::from_millis(20), "u").await,
            Ok(7)
        );
    }

    fn obs_at(t: &str) -> BuildObservation {
        BuildObservation {
            checked_at: t.into(),
            ..BuildObservation::default()
        }
    }

    /// A full check that began before a one-build check, and finished after
    /// it, must not put its older observation over the newer one.
    #[test]
    fn a_full_check_keeps_newer_single_build_observations() {
        let began = parse_time("2026-09-26T10:00:00Z").unwrap();
        let mut s = Stored::default();
        // Build 1 checked on its own while the full check ran; build 2 not;
        // build 3 deleted before; build 4 created and checked meanwhile.
        s.builds.insert(1, obs_at("2026-09-26T10:00:30Z"));
        s.builds.insert(2, obs_at("2026-09-26T04:00:00Z"));
        s.builds.insert(3, obs_at("2026-09-26T04:00:00Z"));
        s.builds.insert(4, obs_at("2026-09-26T10:00:40Z"));
        let observed: HashMap<i64, BuildObservation> = [
            (1, obs_at("2026-09-26T10:00:01Z")),
            (2, obs_at("2026-09-26T10:00:01Z")),
        ]
        .into();
        let checked: BTreeSet<i64> = [1, 2].into();
        merge_full_check(&mut s, &checked, observed, began);
        assert_eq!(s.builds[&1].checked_at, "2026-09-26T10:00:30Z");
        assert_eq!(s.builds[&2].checked_at, "2026-09-26T10:00:01Z");
        assert!(!s.builds.contains_key(&3));
        assert_eq!(s.builds[&4].checked_at, "2026-09-26T10:00:40Z");

        // And the one-build check that ends last with an older observation
        // does not replace a newer full check's either.
        store_build(&mut s, 2, obs_at("2026-09-26T09:59:59Z"));
        assert_eq!(s.builds[&2].checked_at, "2026-09-26T10:00:01Z");
        store_build(&mut s, 2, obs_at("2026-09-26T10:05:00Z"));
        assert_eq!(s.builds[&2].checked_at, "2026-09-26T10:05:00Z");
    }

    /// Each writer serializes when its turn comes, so the row ends as the
    /// state after the last change, whatever order the writes interleave in.
    #[tokio::test]
    async fn the_stored_row_ends_as_the_newest_state() {
        let state = AppState::init_for_tests().await.unwrap();
        let updates = state.builds.updates();
        updates.loaded(&state).await;
        let writers = (0..20).map(|i| {
            let state = state.clone();
            async move {
                let updates = state.builds.updates();
                updates
                    .lock()
                    .builds
                    .insert(i, obs_at("2026-09-26T10:00:00Z"));
                updates.persist(&state).await;
            }
        });
        futures::future::join_all(writers).await;
        let raw = store::get_kv(&state.db, KV_KEY).await.unwrap().unwrap();
        let stored: Stored = serde_json::from_str(&raw).unwrap();
        assert_eq!(stored, updates.lock().clone());
        assert_eq!(stored.builds.len(), 20);
    }

    #[test]
    fn registry_observations_evaluate_against_local_digests() {
        let d = |c: char| format!("sha256:{}", c.to_string().repeat(64));
        let obs = ImageObservation {
            checked_at: "t".into(),
            reference: "ghcr.io/o/r:tag".into(),
            remote_digest: Some(d('i')),
            members: vec![d('m'), d('n')],
            ..ImageObservation::default()
        };
        let local = |c: char| vec![format!("ghcr.io/o/r@{}", d(c))];
        assert!(!obs.evaluate(&local('i')).update_available, "same index");
        assert!(!obs.evaluate(&local('m')).update_available, "its member");
        let u = obs.evaluate(&local('o'));
        assert!(u.update_available);
        assert_eq!(u.local_digests, vec![d('o')]);
        let u = obs.evaluate(&["ghcr.io/o/other@sha256:x".to_string()]);
        assert!(!u.update_available);
        assert!(u.error.unwrap().contains("no registry digest"));
    }
}
