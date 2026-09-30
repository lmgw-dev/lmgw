//! The build data model (container-builds design §3, §4).
//!
//! The row shapes themselves are defined once, in
//! [`lmgw_api_types::builds`], and used here as the domain types: the server
//! stores exactly what the Backends page edits, and a run's `inputs` embeds a
//! [`BuildSpec`] verbatim, so a server-side twin would only be a second
//! definition to keep in step. What lives here is what only the server needs:
//! the insert and patch shapes of a run.

pub use lmgw_api_types::builds::{
    Build, BuildEdit, BuildExtra, BuildRun, BuildRunInputs, BuildRunStatus, BuildSpec,
    BuildTrigger, EditRole, Engine, Forge, GpuBackend, ResolvedExtra, ResolvedInputs,
    DEFAULT_CCACHE_MAX_SIZE,
};

/// A run as it starts: `running`, with the build's definition snapshotted
/// into `inputs.config`. Everything resolved later arrives through
/// [`BuildRunPatch`].
#[derive(Debug, Clone)]
pub struct NewBuildRun {
    pub build_id: i64,
    pub job_id: Option<i64>,
    pub slug: String,
    pub engine: Engine,
    pub trigger: BuildTrigger,
    pub inputs: BuildRunInputs,
    pub log_path: Option<String>,
}

impl NewBuildRun {
    /// A fresh run of `build` as it is right now.
    pub fn of(build: &Build, trigger: BuildTrigger) -> Self {
        Self {
            build_id: build.id,
            job_id: None,
            slug: build.spec.slug.clone(),
            engine: build.spec.engine,
            trigger,
            inputs: BuildRunInputs {
                config: build.spec.clone(),
                resolved: None,
                cfg_hash: None,
            },
            log_path: None,
        }
    }
}

/// A sparse update of a run in flight: `None` leaves a column as it is. The
/// terminal transition is [`crate::store::finish_build_run`], which also
/// stamps `finished_at`, and the moving-tag marker is
/// [`crate::store::promote_build_run`], which keeps it unique per build.
#[derive(Debug, Clone, Default)]
pub struct BuildRunPatch {
    /// A non-terminal status only (`running`); the store refuses a terminal
    /// one here, since that is what `finish_build_run` is for.
    pub status: Option<BuildRunStatus>,
    pub job_id: Option<i64>,
    pub inputs: Option<BuildRunInputs>,
    pub base_sha: Option<String>,
    pub cfg_hash: Option<String>,
    pub image_id: Option<String>,
    pub tags: Option<Vec<String>>,
    pub size_bytes: Option<u64>,
    pub verify: Option<serde_json::Value>,
    pub error: Option<String>,
    pub log_path: Option<String>,
}
