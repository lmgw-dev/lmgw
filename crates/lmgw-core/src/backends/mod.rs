//! Container builds from git — the Backends page (container-builds design).
//!
//! A **build** is a saved, editable definition: a repository and ref, an
//! ordered list of extras merged on top, the GPU backend and the build knobs.
//! Running one is a **build run** — fetch, merge, `podman build`, verify, tag
//! — recorded append-only with everything it resolved. podman stays the source
//! of truth for images; each image lmgw builds carries its provenance in
//! `dev.lmgw.*` labels.
//!
//! This module holds the pieces every later layer stands on:
//!
//! - [`model`] — the row types (shared with the UI through
//!   `lmgw_api_types::builds`) and the run insert/patch shapes; the CRUD is in
//!   [`crate::store`] beside every other table's.
//! - [`validate`] — pure checks for every field of a definition.
//! - [`tags`] — the moving and immutable tags and the config hash behind the
//!   latter.
//! - [`paths`] — where builds keep their files, and the tmpfs guard.
//! - [`presets`] — the engine presets: repositories, per-Dockerfile edits,
//!   targets, build args and verify probes, and the host GPU facts behind
//!   "auto".
//! - [`git`] — the hardened git layer: one object pool, ref resolution and
//!   fetch, per-run worktrees, and assembling base + extras (§14.1).
//!
//! The run executor and the forge clients build on these; [`updates`] (with
//! the registry client in [`oci`]) tells when a build or a registry image has
//! something newer, and [`pull`] pulls a registry image's update.

pub mod forge;
pub mod git;
pub mod images;
pub mod model;
pub mod oci;
pub mod paths;
pub mod presets;
pub mod pull;
pub mod run;
pub mod tags;
pub mod updates;
pub mod validate;

pub use model::*;

pub use lmgw_api_types::builds::ForgePr;

/// The forge facts a build run asks for (§5 phase 1, §14.1): one PR's (or
/// MR's) state — `merged_at`, head and base SHA.
///
/// A trait here rather than a call into the forge client so the run executor
/// neither depends on HTTP nor waits for it in a test. The gateway starts with
/// [`NoForgeLookup`]; the real client is installed with
/// [`run::BuildSeams::set_forge`]. An `Err` is never fatal to a run: the PR's
/// state is then *unknown* — it is merged rather than skipped
/// (`merged_upstream = false`), and a squash-apply takes its fork point from
/// git — and the reason goes into the run log.
#[async_trait::async_trait]
pub trait ForgeLookup: Send + Sync {
    async fn pr(&self, repo_url: &str, forge: Forge, number: u64) -> Result<ForgePr, String>;
}

/// The lookup a gateway has before a forge client is installed: it knows
/// nothing, and says so.
pub struct NoForgeLookup;

#[async_trait::async_trait]
impl ForgeLookup for NoForgeLookup {
    async fn pr(&self, _repo_url: &str, _forge: Forge, number: u64) -> Result<ForgePr, String> {
        Err(format!(
            "no forge client is installed, so PR #{number}'s state (merged upstream?) is unknown"
        ))
    }
}
