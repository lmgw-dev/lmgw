//! Container builds from git (container-builds design §3–§5): the **build**,
//! an editable definition of what to build, and the **build run**, one
//! append-only execution of it.
//!
//! Shared rather than mirrored, for the reason [`image_lab`](crate::image_lab)
//! and [`scope`](crate::scope) are: the server stores exactly these shapes (a
//! run's `inputs` embeds the build definition as it was built) and the
//! Backends page edits them, so a second definition on either side would be
//! the first thing to drift. Validation, tags and the config hash stay
//! server-side (`lmgw_core::backends`). The one piece of tag logic here is the
//! engine → image repository mapping and the moving tag built from it, which
//! the page needs in order to show what a build "follows".
//!
//! "Recipe" is deliberately not used anywhere in this vocabulary: it already
//! names the sd.cpp model recipes (§1).

use serde::{Deserialize, Serialize};

/// Which of the three engines lmgw runs a build produces (§3). ik_llama.cpp is
/// `Llama` with its own repo preset, not an engine of its own: its images run
/// in the same classes.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Engine {
    /// llama.cpp and ik_llama.cpp — `llama-server`.
    #[default]
    Llama,
    /// audio.cpp — `audiocpp_server`.
    Audio,
    /// stable-diffusion.cpp — `sd-server`.
    Sdcpp,
}

impl Engine {
    pub const ALL: [Engine; 3] = [Engine::Llama, Engine::Audio, Engine::Sdcpp];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Llama => "llama",
            Self::Audio => "audio",
            Self::Sdcpp => "sdcpp",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "llama" => Some(Self::Llama),
            "audio" => Some(Self::Audio),
            "sdcpp" => Some(Self::Sdcpp),
            _ => None,
        }
    }

    /// The repository every image of this engine is tagged into (§5 "Tags",
    /// §11.2): a namespace of lmgw's own, so nothing here ever retags the
    /// hand-built `localhost/llama-server-cuda:*` images.
    pub const fn image_repo(self) -> &'static str {
        match self {
            Self::Llama => "localhost/lmgw-llama-server",
            Self::Audio => "localhost/lmgw-audio-cpp",
            Self::Sdcpp => "localhost/lmgw-sd-server",
        }
    }

    /// The lmgw classes that may run an image of this engine (§3): what the
    /// image picker filters suggestions by.
    pub fn classes(self) -> &'static [&'static str] {
        match self {
            Self::Llama => &["chat", "aux"],
            Self::Audio => &["audio"],
            Self::Sdcpp => &["image"],
        }
    }
}

/// The moving tag of a build, `<engine repo>:<slug>` (§5 "Tags"). Picking it
/// in a class-image field means "follow this build"; each run's immutable tag
/// (`<repo>:<slug>-<base7>-<cfg6>`, computed server-side from the resolved
/// inputs) means "pin this exact image".
pub fn moving_tag(engine: Engine, slug: &str) -> String {
    format!("{}:{slug}", engine.image_repo())
}

/// Where a build's repository lives, which decides how PR extras are found
/// (§4, §7).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Forge {
    /// PRs are `refs/pull/<n>/head`, details from the GitHub REST API.
    Github,
    /// MRs are `refs/merge-requests/<n>/head`, details from `/api/v4`.
    Gitlab,
    /// No PR list and no forge API: refs from other remotes only.
    #[default]
    Plain,
}

impl Forge {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Plain => "plain",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "github" => Some(Self::Github),
            "gitlab" => Some(Self::Gitlab),
            "plain" => Some(Self::Plain),
            _ => None,
        }
    }
}

/// The GPU backend a build targets (§4). Labelled **GPU backend** on the
/// Backends page, so it does not read as the page's own subject. Picks the
/// preset's Dockerfile candidate.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum GpuBackend {
    #[default]
    Cuda,
    Vulkan,
    Rocm,
    Cpu,
}

impl GpuBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cuda => "cuda",
            Self::Vulkan => "vulkan",
            Self::Rocm => "rocm",
            Self::Cpu => "cpu",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cuda" => Some(Self::Cuda),
            "vulkan" => Some(Self::Vulkan),
            "rocm" => Some(Self::Rocm),
            "cpu" => Some(Self::Cpu),
            _ => None,
        }
    }
}

/// One entry of a build's ordered extras list (§4): something merged on top
/// of the base ref, in order.
///
/// With no `pin`, a run follows the head. A pin is a full commit SHA and
/// builds exactly that commit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BuildExtra {
    /// A PR (GitHub) or MR (GitLab) of the build's own `repo_url`.
    Pr {
        number: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pin: Option<String>,
    },
    /// A branch, tag or commit of another remote: a fork branch, an upstream
    /// llama.cpp PR on top of ik via `refs/pull/N/head`, …
    Ref {
        remote_url: String,
        #[serde(rename = "ref")]
        git_ref: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pin: Option<String>,
    },
}

impl BuildExtra {
    pub fn pin(&self) -> Option<&str> {
        match self {
            Self::Pr { pin, .. } | Self::Ref { pin, .. } => pin.as_deref(),
        }
    }

    /// `pr` | `ref` — the serde tag, for messages and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Pr { .. } => "pr",
            Self::Ref { .. } => "ref",
        }
    }

    /// How the extra is named in a log line or an error: `PR #1234`,
    /// `https://github.com/fork/llama.cpp feature-x`.
    pub fn label(&self) -> String {
        match self {
            Self::Pr { number, .. } => format!("PR #{number}"),
            Self::Ref {
                remote_url,
                git_ref,
                ..
            } => format!("{remote_url} {git_ref}"),
        }
    }
}

/// What a Dockerfile edit is for (§14.2). The role is what lets the build's
/// own switches reach an edit that was customized: `ccache` off drops every
/// [`Ccache`](Self::Ccache) and [`Cache`](Self::Cache) edit, whether it came
/// from the preset or was typed by hand.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum EditRole {
    /// Compiler-cache wiring: the `ccache` package, the cache mount, the
    /// compiler launchers, the stats line.
    Ccache,
    /// Another build cache mount (the web UI's npm cache). Optional, and a
    /// cache like ccache: off with it.
    Cache,
    /// Build number / commit / date injection, so `--version` names what was
    /// built even though `.git` is not in the build context.
    BuildInfo,
    /// `docker.io/` in front of a bare base image, so it resolves without a
    /// short-name alias.
    Qualify,
    /// A different base-image variant (sd.cpp's `-cudnn-` dropped).
    BaseImage,
    /// Anything else — what a hand-written edit is unless it says otherwise.
    #[default]
    Other,
}

impl EditRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ccache => "ccache",
            Self::Cache => "cache",
            Self::BuildInfo => "build_info",
            Self::Qualify => "qualify",
            Self::BaseImage => "base_image",
            Self::Other => "other",
        }
    }

    /// Whether the edit only makes sense with the build's cache switch on.
    pub fn needs_ccache(self) -> bool {
        matches!(self, Self::Ccache | Self::Cache)
    }
}

/// One Dockerfile edit (§4, §5 step 4, §14.2): a literal find/replace of
/// **every** occurrence, applied to a copy of the Dockerfile, never to the
/// checkout, in list order (a later edit sees what the earlier ones wrote).
/// Each one is logged as applied or not matched; a `required` edit that does
/// not match fails the run.
///
/// `find` and `replace` may carry `{{ccache_id}}`, `{{npm_cache_id}}`,
/// `{{ccache_max_size}}`, `{{ccache_shared_mount}}`/`{{ccache_seed}}` and
/// `{{npm_shared_mount}}`/`{{npm_seed}}`, filled in per run, so an edit list
/// stays the same when only a cache id or the size changes. The last two
/// pairs render empty for a plain build; with extras they mount the shared
/// ccache/npm cache read-only and seed the build's own from it. `name` and
/// `role` are for the log and the ccache switch; neither changes the image,
/// so neither is hashed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildEdit {
    /// Short label for the log and the editor (`ccache-mount`). May be empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub role: EditRole,
    pub find: String,
    pub replace: String,
    pub required: bool,
}

/// `CCACHE_MAXSIZE` a build gets unless it says otherwise — `build.sh`'s
/// value (§4).
pub const DEFAULT_CCACHE_MAX_SIZE: &str = "10G";

/// Every editable field of a build (§4) — the `builds` row minus its id and
/// timestamps. What the editor sends, what `duplicate` copies, and what a run
/// snapshots into its `inputs` as "the config as built".
///
/// `None` on the optional fields means **auto**, resolved by the run from the
/// engine preset and the host, and the resolved value is recorded on the run:
/// `cuda_version` → the preset default, `arch` → the host GPUs' compute
/// capabilities, `dockerfile`/`target` → the preset's candidates, `edits` →
/// the preset's edits for the resolved Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildSpec {
    /// Becomes the tag. Tag charset, bounded so the longest immutable tag
    /// fits 128 characters, and immutable once the build has a run.
    pub slug: String,
    pub name: String,
    pub engine: Engine,
    /// A preset or any git URL (`https://`, `http://`, `ssh://`,
    /// `git@host:path`, `file://`).
    pub repo_url: String,
    pub forge: Forge,
    /// Branch, tag or commit to build on.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Merged on top of `git_ref`, in order.
    pub extras: Vec<BuildExtra>,
    pub backend: GpuBackend,
    /// `None` = the preset's default for the engine.
    pub cuda_version: Option<String>,
    /// Compute capabilities / GPU targets (`["89"]`, `["86", "89"]`,
    /// `["gfx1100"]`). `None` = auto-detect from the host GPUs.
    pub arch: Option<Vec<String>>,
    /// Repo-relative Dockerfile path. `None` = the preset's candidates.
    pub dockerfile: Option<String>,
    /// Final build stage. `None` = the preset's candidates.
    pub target: Option<String>,
    /// `None` = the preset's edits for the resolved Dockerfile; `Some` = this
    /// explicit, customized list (which may be empty: no edits at all).
    pub edits: Option<Vec<BuildEdit>>,
    pub ccache: bool,
    /// `CCACHE_MAXSIZE`, e.g. `10G`, `500M`; `0` is ccache's "no limit".
    pub ccache_max_size: String,
    /// `--cpuset-cpus` (`0-15`, `0,2,4`). `None` = all cores.
    pub cpus: Option<String>,
    /// Extra `--build-arg` lines, one `KEY=VALUE` per line.
    pub build_args: String,
    /// Keep podman's layer cache (`--layers=true`). Off by default: no
    /// multi-GB intermediates are left behind.
    pub keep_layers: bool,
    /// How many previous runs' immutable tags to keep. `None` = keep all.
    pub keep_runs: Option<u32>,
    pub notes: String,
}

impl Default for BuildSpec {
    fn default() -> Self {
        Self {
            slug: String::new(),
            name: String::new(),
            engine: Engine::default(),
            repo_url: String::new(),
            forge: Forge::default(),
            git_ref: String::new(),
            extras: Vec::new(),
            backend: GpuBackend::default(),
            cuda_version: None,
            arch: None,
            dockerfile: None,
            target: None,
            edits: None,
            ccache: true,
            ccache_max_size: DEFAULT_CCACHE_MAX_SIZE.to_string(),
            cpus: None,
            build_args: String::new(),
            keep_layers: false,
            keep_runs: None,
            notes: String::new(),
        }
    }
}

/// One `builds` row: the editable definition plus its identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Build {
    pub id: i64,
    #[serde(flatten)]
    pub spec: BuildSpec,
    pub created_at: String,
    pub updated_at: String,
}

impl Build {
    /// This build's moving tag — see [`moving_tag`].
    pub fn moving_tag(&self) -> String {
        moving_tag(self.spec.engine, &self.spec.slug)
    }
}

/// Where a build run is, or how it ended (§5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BuildRunStatus {
    /// Still going — including waiting for the machine-wide build lock, which
    /// the job detail names.
    #[default]
    Running,
    /// Built, verified and promoted: the moving tag points at its image.
    Succeeded,
    /// Built, but not GPU-verified (GPU hold, or a CUDA OOM while probing).
    /// Not promoted; **Verify now** can finish the job later.
    Unverified,
    /// Built, and a verify check failed. Not promoted.
    Broken,
    /// Did not produce an image.
    Failed,
    Canceled,
    /// The immutable tag for these exact inputs already existed and was
    /// verified; nothing was built.
    UpToDate,
}

impl BuildRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Unverified => "unverified",
            Self::Broken => "broken",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::UpToDate => "up_to_date",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "unverified" => Some(Self::Unverified),
            "broken" => Some(Self::Broken),
            "failed" => Some(Self::Failed),
            "canceled" => Some(Self::Canceled),
            "up_to_date" => Some(Self::UpToDate),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    /// Whether the run left an image behind (verified or not).
    pub fn built(self) -> bool {
        matches!(self, Self::Succeeded | Self::Unverified | Self::Broken)
    }
}

/// What started a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BuildTrigger {
    /// A click on the Backends page (or the `/api` op behind it).
    #[default]
    Manual,
    /// `lmgw__build_run`.
    Mcp,
    /// A scheduled run.
    Schedule,
}

impl BuildTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Mcp => "mcp",
            Self::Schedule => "schedule",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "manual" => Some(Self::Manual),
            "mcp" => Some(Self::Mcp),
            "schedule" => Some(Self::Schedule),
            _ => None,
        }
    }
}

/// One extra with the commit it resolved to for a run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResolvedExtra {
    pub extra: BuildExtra,
    /// Full commit SHA that was (or will be) merged.
    pub sha: String,
}

/// What every "auto" and every "follow the head" of a build became for one
/// run (§5 steps 1–4). Filled by the run executor; the config hash is computed
/// over these values, never over the unresolved definition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResolvedInputs {
    /// Full SHA the base ref resolved to.
    pub base_sha: String,
    /// The extras the run merges, in order, each with its resolved SHA. A
    /// `pr` extra already merged upstream is skipped by the run and left out
    /// here — it does not change the image.
    pub extras: Vec<ResolvedExtra>,
    /// `None` for a non-CUDA backend.
    pub cuda_version: Option<String>,
    /// The arch list passed to the build, in the order passed.
    pub arch: Vec<String>,
    /// Repo-relative Dockerfile path.
    pub dockerfile: String,
    /// Final build stage.
    pub target: String,
    /// The edits applied — the build's explicit list, or the preset's for
    /// `dockerfile`.
    pub edits: Vec<BuildEdit>,
}

/// A run's `inputs` column: the full snapshot (§3 "Build run").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildRunInputs {
    /// The build definition exactly as it was when the run started — a run
    /// outlives edits to its build, and the build itself.
    pub config: BuildSpec,
    /// `None` until the run has resolved its refs (phase 1).
    pub resolved: Option<ResolvedInputs>,
    /// Full sha256 hex of the image-affecting inputs; the immutable tag
    /// carries its first 6 characters. `None` until resolved.
    pub cfg_hash: Option<String>,
}

/// One `build_runs` row (§3 "Build run"): append-only, and kept when its
/// build is deleted (`build_id` becomes `None`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildRun {
    pub id: i64,
    /// `None` once the build has been deleted.
    pub build_id: Option<i64>,
    /// The background job that executed it (`JobKind::BuildRun`). Not a
    /// foreign key: job rows are pruned by the jobs retention settings, runs
    /// are not.
    pub job_id: Option<i64>,
    /// Snapshot of the build's slug and engine, so a run stays readable after
    /// its build is gone.
    pub slug: String,
    pub engine: Engine,
    pub trigger: BuildTrigger,
    pub status: BuildRunStatus,
    pub inputs: BuildRunInputs,
    pub base_sha: Option<String>,
    pub cfg_hash: Option<String>,
    /// Full image ID (no `sha256:`).
    pub image_id: Option<String>,
    /// Every tag this run put on its image (immutable, and the moving tag
    /// when promoted).
    pub tags: Vec<String>,
    /// lmgw last moved the build's moving tag to this run's image. At most
    /// one run per build carries it. podman is the source of truth: a tag
    /// moved by hand is not reflected here.
    pub promoted: bool,
    pub size_bytes: Option<u64>,
    /// The verify checks' results (§5 step 6), shaped by the executor.
    pub verify: Option<serde_json::Value>,
    pub error: Option<String>,
    /// The full, never-truncated build log.
    pub log_path: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

// ---------------------------------------------------------------------------
// API contract (container-builds design §15): op args
// ---------------------------------------------------------------------------
//
// Every args struct refuses a field it does not know (`deny_unknown_fields`):
// a typo'd argument is an error that names it, never a silent default.

/// `build_get` args: one build's detail plus a page of its run history,
/// newest first (§15). `limit` is explicit — the UI pages with ShowMore
/// rather than an unbounded fetch. `before` pages further back: the id of
/// the oldest run already shown, so the next page starts strictly older
/// than it. It is an exclusive run-id cursor and need not name an existing
/// run: `before = run_id + 1` with `limit = 1` fetches exactly `run_id`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildsGetArgs {
    pub id: i64,
    pub limit: Option<u32>,
    pub before: Option<i64>,
}

/// `build_set`'s `action` (§15).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BuildSetAction {
    #[default]
    Create,
    Update,
    Delete,
    Duplicate,
}

/// `build_set` args (§15): create, update, delete or duplicate a build.
/// `spec` carries the fields for `create`/`update`; `slug`/`name` rename the
/// copy on `duplicate`; `delete_images` extends `delete` to also remove the
/// build's images (refused while any is in use, like
/// [`ContainerImageDeleteArgs`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildSetArgs {
    pub action: BuildSetAction,
    pub id: Option<i64>,
    pub spec: Option<BuildSpec>,
    pub slug: Option<String>,
    pub name: Option<String>,
    pub delete_images: bool,
}

/// `build_resolve` args (§15): preview what a spec resolves to right now,
/// without building — the build editor's "Resolve" button.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildResolveArgs {
    pub spec: BuildSpec,
}

/// `build_check_merge` args (§15, §7, §14.1): either a saved build (`id`) or
/// an unsaved spec straight from the editor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildCheckMergeArgs {
    pub id: Option<i64>,
    pub spec: Option<BuildSpec>,
}

/// `build_run` args (§15, §5 step 2). `rebuild` forces `--pull=newer` and a
/// full build even when the immutable tag already exists and is verified
/// ("Rebuild anyway").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildRunArgs {
    pub id: i64,
    pub rebuild: bool,
}

/// `build_run_log` args (§15, §5 step 5): a byte-offset chunk of the run's
/// log file (at most 1 MiB per reply). The UI polls with the previous
/// response's `next_offset`, only while `done` is false.
///
/// `tail` asks for the log's last `tail` lines instead — how a long run
/// ended, without paging through it from the start; the reply's
/// `next_offset` continues from there. It cannot be combined with a non-zero
/// `offset`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildRunLogArgs {
    pub run_id: i64,
    pub offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<usize>,
}

/// `build_promote` args (§15, §6 "Make current" / rollback): move the
/// build's moving tag to this run's image.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildPromoteArgs {
    pub run_id: i64,
}

/// `build_verify` args (§15, §5 step 6): re-run the help/device-probe checks
/// for a run that was built but not GPU-verified ("Verify now").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildVerifyArgs {
    pub run_id: i64,
}

/// `forge_refs` args (§15, §4 "ref"): branch/tag suggestions for the ref
/// combobox.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ForgeRefsArgs {
    pub repo_url: String,
}

/// `forge_prs` args (§15, §7): one page of the extras picker's PR/MR list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ForgePrsArgs {
    pub repo_url: String,
    pub forge: Forge,
    pub query: Option<String>,
    pub page: Option<u32>,
}

/// `forge_pr` args (§15, §5 step 1 "pr extras also get forge state"): one
/// PR/MR's current forge state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ForgePrArgs {
    pub repo_url: String,
    pub forge: Forge,
    pub number: u64,
}

/// `container_images` args (§15, §9.1 Images tab): every local image, or
/// only one engine's.
///
/// `disk: false` skips the disk footer — `podman system df` and the scan for
/// buildah leftovers, most of this op's time — for a caller that shows no
/// footer (the image picker); the response then says so in
/// [`ContainerImagesResponse::disk_skipped`]. Absent means `true`, as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImagesArgs {
    pub engine: Option<Engine>,
    pub disk: bool,
}

impl Default for ContainerImagesArgs {
    fn default() -> Self {
        Self {
            engine: None,
            disk: true,
        }
    }
}

/// `container_image_delete` args (§15): `image` is a tag or an ID. While
/// `used_by` is non-empty it is refused (the error lists the users and what
/// `force` would do) unless `force` is set; refused on a dev instance (§10)
/// either way.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImageDeleteArgs {
    pub image: String,
    /// Delete it even though something uses it — what the user confirmed
    /// after being shown the users. Every container on the image, running or
    /// stopped, is stopped and removed first (a model's through the runtime,
    /// so a running model goes down the way the Stop button takes it, in-flight
    /// requests and all). Class defaults and model overrides that name it are
    /// left alone: they name a missing image afterwards, and the answer lists
    /// them.
    pub force: bool,
}

/// `container_image_tag` args (§15): add and/or remove one tag on an image.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImageTagArgs {
    pub image: String,
    pub add: Option<String>,
    pub remove: Option<String>,
}

/// `build_updates_check` args (§15, §8, WP6): one build, or every build when
/// `id` is absent. Without `id` the registry images in use are checked too
/// (their results are on `container_images`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildUpdatesCheckArgs {
    pub id: Option<i64>,
}

/// `container_image_pull` args (§8 **Pull update**): the registry reference
/// to pull — normally a [`RegistryUpdate::reference`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImagePullArgs {
    pub image: String,
}

/// `container_image_pull_status` args: the `image_pull` job to read — a
/// [`ContainerImagePullStarted::job_id`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImagePullStatusArgs {
    pub job_id: i64,
}

// ---------------------------------------------------------------------------
// API contract (container-builds design §15): responses
// ---------------------------------------------------------------------------

/// How something is using an image (§15): what
/// [`ContainerImageDeleteArgs`] refuses to delete out from under, and what
/// the promote success panel (§6) offers to recreate.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ImageUseKind {
    #[default]
    ClassDefault,
    ModelOverride,
    RunningContainer,
    /// A container that exists but is not running — exited, created, or a
    /// buildah working container (`podman ps -a --external`). It keeps its
    /// image from being removed all the same.
    StoppedContainer,
}

/// One user of an image (§15 `ImageUse`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageUse {
    pub kind: ImageUseKind,
    /// The lmgw class (`chat` | `aux` | `audio` | `image` —
    /// `lmgw_core::runtime::Class::as_str`).
    pub class: String,
    /// Which model row: set for `model_override`, and for a
    /// `running_container` or `stopped_container` that is one of lmgw's model
    /// containers (read from its labels, else matched by its name) — with
    /// `class`, what the `container` op's `apply` recreates. `None` for a
    /// container that is no model's (an agent's, or anything else on the
    /// machine).
    pub model_id: Option<String>,
    /// Set for `running_container` and `stopped_container`: its name.
    pub container: Option<String>,
}

/// The largest `build_update_check_hours` the server accepts (§8): `0` (off)
/// up to a year. Shared so the Settings page's (`lmgw-ui`) client-side hint
/// and validation state and check the same number the server enforces
/// (`lmgw_core::backends::updates::validate_check_hours`), never a
/// separately invented UI limit.
pub const MAX_BUILD_UPDATE_CHECK_HOURS: u32 = 8760;

/// A build's update badge state (§15, §8): the result of the periodic (or
/// "Check now") comparison of the resolved ref and extras against the last
/// verified run.
///
/// `Some` on a [`BuildView`] once the build has been checked against a run it
/// can be compared with; `None` before its first check, and for a build with
/// no succeeded (or unverified) run. A status with no `reasons` is **up to
/// date as of `checked_at`** — [`Self::has_update`] is what the Backends nav
/// badge counts. What could not be checked is in `errors`, never in `reasons`:
/// a rate-limited forge is not an update.
///
/// The server re-evaluates the stored remote facts against the build and its
/// runs on every read, so a run that builds the new head clears "master moved"
/// at once, and an edit shows "definition changed since last run" at once —
/// neither waits for the next check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpdateStatus {
    /// When the remote facts behind this status were fetched (RFC 3339).
    pub checked_at: String,
    /// Human sentences, one per reason (§8: `"master +37 commits"`,
    /// `"master moved (abc1234 → def5678)"` when the count is not known
    /// without fetching, `"PR #1234 pushed"`, `"PR #1234 closed unmerged"`,
    /// `"definition changed since last run (ref, extras)"`). A PR the forge
    /// reports merged reads `"PR #1234 merged upstream and contained in your
    /// base — drop it"` only once the base is proven (locally, no fetch) to
    /// already hold its merge or head commit; otherwise `"PR #1234 merged
    /// upstream at <date>; your base doesn't contain it yet — it's still
    /// merged into your build"` — the same non-committal wording whether that
    /// was proven false or is simply unknown.
    pub reasons: Vec<String>,
    /// The base `ref` itself moved (a branch or tag; a pinned commit never
    /// updates, §8).
    pub ref_moved: bool,
    pub extras: Vec<ExtraChange>,
    /// What the check could not find out, one sentence each, starting
    /// `check failed:` — a forge's rate limit (with its reset time), a remote
    /// that did not answer, a ref that no longer exists. Shown, never counted
    /// as an update.
    pub errors: Vec<String>,
}

impl UpdateStatus {
    /// Something newer (or different) than the compared run is there to
    /// build — what the nav badge counts.
    pub fn has_update(&self) -> bool {
        !self.reasons.is_empty()
    }
}

/// One unpinned extra's change since the last verified run (§15, §8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExtraChange {
    pub label: String,
    /// The badge's own wording (§8): `"pushed"`, `"closed unmerged"`, or one
    /// of the two "merged upstream" readings ([`UpdateStatus::reasons`]).
    pub change: String,
}

/// One row of the Builds tab (§15, §9.1): a build plus what it is doing and
/// what is using it right now. `current_run` is the promoted run — its
/// image holds the moving tag; `last_run` is simply the most recent one,
/// which may be a failed or unverified attempt after `current_run`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildView {
    pub build: Build,
    pub last_run: Option<BuildRun>,
    pub current_run: Option<BuildRun>,
    /// The `JobRow.id` of this build's run while one is live.
    pub live_job_id: Option<i64>,
    pub used_by: Vec<ImageUse>,
    pub update: Option<UpdateStatus>,
    /// The build's moving tag **on the answering instance**: [`moving_tag`]
    /// in production, `localhost/lmgw-dev-<engine repo>:<slug>` on a dev
    /// instance (which tags into a namespace of its own). Computed by the
    /// server, the same way its runs tag, so it is right for a build that
    /// has never run too — prefer it to [`Build::moving_tag`].
    pub moving_tag: String,
}

/// `builds` response (§15): every build, for the Builds tab.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildsResponse {
    pub builds: Vec<BuildView>,
    /// Set when who uses which image could not be determined (podman did not
    /// answer). Every `used_by` is then empty **for that reason** — not
    /// because nothing uses the image — and the page should say so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_error: Option<String>,
}

/// `build_get` response (§15): one build's detail plus a page of its run
/// history, newest first. `more` says whether an older page exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildGetResponse {
    pub view: BuildView,
    pub runs: Vec<BuildRun>,
    pub more: bool,
    /// As [`BuildsResponse::usage_error`], for `view.used_by`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_error: Option<String>,
}

/// A tag that was left in place, and why: one a `keep_runs` prune kept (§5
/// step 7 "each one kept is logged with the reason"), or one a `build_set`
/// delete with `delete_images` did not remove (in use, a name from outside
/// the build, a dev instance).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KeptImage {
    pub tag: String,
    pub reason: String,
}

/// `build_set` response (§15). `build` is `None` after a `delete`. With
/// `delete_images`, `removed_images` lists the tags actually removed (an
/// image goes with its last one) and `kept` the tags left in place, each with
/// its reason; both are empty otherwise. A `delete` (regardless of
/// `delete_images`) also removes the build's own per-build ccache and npm
/// cache directories (§14.2) and lists them in `removed_caches`, each entry
/// naming the cache-mount id and, when it could be read for free, its size
/// (`"lmgw-llama-cuda-master-pr1 (572.1 MiB)"`) — empty when the build never
/// had one (no extras were ever built), a run of it was live (the delete
/// itself is refused then) or this is a dev instance, whose `/var/tmp` is
/// production's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildSetResponse {
    pub build: Option<Build>,
    pub removed_images: Vec<String>,
    pub kept: Vec<KeptImage>,
    pub removed_caches: Vec<String>,
}

/// `build_resolve` response (§15): what a spec resolves to right now — the
/// editor's "Resolve" button and tag preview.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResolvedPreview {
    pub base_sha: String,
    /// `rev-list --count <base>` (§14.1) — `llama` engines only; `None` for
    /// `audio`/`sdcpp`.
    pub build_number: Option<u64>,
    pub dockerfile: String,
    pub target: String,
    /// The matched `DockerfileProfile`'s label
    /// (`lmgw_core::backends::presets`).
    pub profile: String,
    /// The preset's edits for the resolved Dockerfile, or the build's own
    /// explicit list — each already carries its [`EditRole`].
    pub edits: Vec<BuildEdit>,
    /// Extra `--build-arg KEY=VALUE` pairs, in order.
    pub build_args: Vec<(String, String)>,
    pub moving_tag: String,
    pub immutable_tag: String,
    /// E.g. the build asks for a newer CUDA than the driver supports (§4).
    pub warnings: Vec<String>,
    /// How each of `edits` fared against the resolved Dockerfile, in the same
    /// order — which matched and how often. Checked against the **base**'s
    /// Dockerfile (a preview merges nothing), so an extra that changes the
    /// Dockerfile can still move a run's result.
    pub edit_outcomes: Vec<EditApplied>,
}

/// One Dockerfile edit's result in a [`ResolvedPreview`]: the run log's
/// "applied (N matches)" / "not matched", as data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EditApplied {
    /// Position in the edit list, from 0 (an edit's `name` may be empty).
    pub index: u32,
    pub name: String,
    pub role: EditRole,
    pub required: bool,
    /// Occurrences replaced; `0` = not matched (a run fails on that when the
    /// edit is `required`).
    pub applied: u32,
}

/// One outcome an extra can have when assembled (§15, §5 step 3, §14.1).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MergeOutcome {
    #[default]
    Merged,
    /// Its head was already an ancestor of `HEAD`, or the merge's diff
    /// against its first parent was empty (§14.1 "squash-merged PRs").
    AlreadyInBase,
    /// Skipped on the forge's word (`merged_at` set, §14.1).
    MergedUpstream,
    /// No shared history with the assembled `HEAD`: applied as a squash of
    /// its changes since its fork point instead of a merge (§14.1).
    SquashApplied,
    /// Only in a [`CheckMergeStep`] or a failed run — assemble stops here
    /// (§5 phase 3 "On a conflict").
    Conflict,
}

/// One extra's row in a [`CheckMergeReport`] (§15, §7 "Check merge").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CheckMergeStep {
    pub label: String,
    pub outcome: MergeOutcome,
    pub sha: String,
    /// Conflicted paths — set only when `outcome` is `conflict`.
    pub files: Vec<String>,
    pub note: String,
}

/// `build_check_merge` response (§15, §7, §14.1): fetches, then merges into
/// a throwaway worktree running the same code as the run's assemble phase,
/// and reports the outcome without building or touching the run's worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CheckMergeReport {
    pub base_sha: String,
    /// `true` when every extra merged cleanly (no `conflict` step).
    pub ok: bool,
    pub steps: Vec<CheckMergeStep>,
}

/// `build_run` response (§15): the background job started, and the run row
/// it will fill in as it goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildRunStarted {
    pub job_id: i64,
    pub run_id: i64,
}

/// `build_run_log` response (§15): a tail of the run's log file starting at
/// the requested byte offset. `next_offset` is what the next poll should
/// send; `done` stops the UI's polling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RunLogChunk {
    pub text: String,
    pub next_offset: u64,
    pub done: bool,
}

/// `build_promote` response (§15, §6 "Make current"): the moving tag's new
/// target, and who was using the old one — the success panel's "Recreate N
/// running containers" / "Set as default for …" offers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PromoteResponse {
    pub moving_tag: String,
    /// `None` when nothing held the moving tag before (a build's first
    /// promotion).
    pub from_image: Option<String>,
    pub to_image: String,
    pub used_by: Vec<ImageUse>,
}

/// `build_verify` response (§15, §5 step 6, §14.3): also updates the run's
/// status (`succeeded`, `unverified` or `broken`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VerifyReport {
    /// The `--help` probe (§14.3) parsed cleanly.
    pub help_ok: bool,
    /// Devices the `--list-devices` (or engine equivalent) probe reported.
    pub devices: Vec<String>,
    /// The device probe matched the build's backend (§14.3's success
    /// pattern). `false` under GPU hold or a CUDA OOM while probing —
    /// "not GPU-verified", not "broken".
    pub gpu_verified: bool,
    pub notes: Vec<String>,
}

/// One entry of the build editor's repository picker (§15, §9.1) — the
/// owned-data mirror, for the wire, of
/// `lmgw_core::backends::presets::RepoPreset`'s `REPO_PRESETS` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RepoPreset {
    pub id: String,
    pub name: String,
    pub engine: Engine,
    pub repo_url: String,
    pub forge: Forge,
    /// The default branch — what a new build's `ref` starts as.
    pub default_ref: String,
}

/// `build_env` response (§15): static and host-detected context the build
/// editor needs, fetched once when it opens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildEnv {
    pub repo_presets: Vec<RepoPreset>,
    /// `DEFAULT_CUDA_VERSION` (§14.2): `13.0.0`, prefilled unless the build
    /// overrides it.
    pub cuda_default: String,
    /// The host GPUs' unique `compute_cap` values (§4 "arch" auto).
    pub arch_auto: Vec<String>,
    /// The driver's max supported CUDA version, for the "asks for more than
    /// the driver supports" warning (§4).
    pub driver_cuda_max: Option<String>,
    /// Whether a working `git` was found (§10 "git dependency").
    pub git_ok: bool,
    pub builds_dir: String,
    /// Set when `builds_dir` is on tmpfs — a dev instance must move it
    /// before it can run builds (§5 "Workspace", §10 "Dev instance").
    pub builds_dir_warning: Option<String>,
    /// Free space on the filesystem of `builds_dir` (or of its nearest
    /// existing ancestor), for the editor's "free disk next to the last run's
    /// footprint" (§9.1 — a warning, never a block). `None` when it could not
    /// be read.
    pub builds_dir_free_bytes: Option<u64>,
}

/// One branch or tag as `git ls-remote` reports it (§15, §4 "ref").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RefEntry {
    pub name: String,
    pub sha: String,
}

/// `forge_refs` response (§15): ref suggestions for the editor's ref
/// combobox, from `git ls-remote --heads --tags` (annotated tags peeled to
/// `^{}`, §14.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RemoteRefsView {
    pub default_branch: String,
    pub heads: Vec<RefEntry>,
    pub tags: Vec<RefEntry>,
}

/// One PR (GitHub) or MR (GitLab) as the extras picker shows it (§15, §7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ForgePr {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub updated_at: String,
    pub draft: bool,
    /// The forge's own spelling (`open` | `closed`).
    pub state: String,
    pub head_sha: String,
    pub base_sha: String,
    /// Set once the forge reports it merged (§5 step 1, §14.1 "the forge's
    /// `merged_at` is the primary signal").
    pub merged_at: Option<String>,
    pub url: String,
    /// The commit the merge put on its target branch — the merge, squash or
    /// rebase commit (GitHub `merge_commit_sha`; GitLab `merge_commit_sha`,
    /// else `squash_commit_sha`). Empty when the forge gives none (an open
    /// PR, an old GitLab). A build skips a merged PR as "merged upstream"
    /// only when its base contains this commit (or the PR's head).
    pub merge_commit_sha: String,
}

/// The forge API's remaining quota (§7: "Without a token GitHub allows 60
/// requests/h. When the limit is hit, the picker shows the reset time
/// instead of an empty list").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RateLimit {
    pub remaining: u32,
    pub reset_at: String,
}

/// `forge_prs` response (§15): one page of the extras picker's PR/MR list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ForgePrPage {
    pub prs: Vec<ForgePr>,
    pub next_page: Option<u32>,
    pub rate_limit: Option<RateLimit>,
}

/// An image's build provenance (§15 `container_images`, §5 "Labels"): read
/// from the `dev.lmgw.*` OCI labels, so an image stays self-describing even
/// after its `builds`/`build_runs` rows are gone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageProvenance {
    pub build_id: i64,
    /// The build's name as it is now — its slug when it has no name — read
    /// from the builds table, so "follows <build>" needs no second call.
    /// `None` once the build is deleted (the labels outlive it).
    pub build_name: Option<String>,
    pub run_id: i64,
    pub slug: String,
    pub repo: String,
    pub git_ref: String,
    /// Base SHA the run built from (`dev.lmgw.base`).
    pub base: String,
    /// The extras merged in, each with its resolved SHA (`dev.lmgw.extras`,
    /// stored on the image as JSON) — reuses [`ResolvedExtra`], the same
    /// shape a run's `inputs.resolved` carries.
    pub extras: Vec<ResolvedExtra>,
    /// The lmgw instance that built it (`dev.lmgw.instance`: one per data
    /// dir, so a dev instance's images are told from production's). Empty
    /// on an image built before the label existed.
    pub instance: String,
    /// Built by another lmgw instance than the one answering. Its build and
    /// run ids are that instance's, so `build_id` and `run_id` are 0 and
    /// `build_name` is `None` — the provenance is the labels' alone.
    pub other_instance: bool,
}

/// One local or external image of the three engines (§15, §9.1 Images tab).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImage {
    /// Full image ID (no `sha256:`).
    pub id: String,
    pub tags: Vec<String>,
    /// `None` when the image can't be matched to an engine.
    pub engine: Option<Engine>,
    /// The `dev.lmgw.backend` label, when present — a free string because
    /// an external image (a `build.sh` tag, a registry pull) may not carry
    /// it at all.
    pub backend: Option<String>,
    pub size: u64,
    pub created: String,
    /// `None` for an external image lmgw did not build (§3 "Image").
    pub provenance: Option<ImageProvenance>,
    /// No `dev.lmgw.run` label — a `build.sh` image or a registry pull
    /// (§3 "Image").
    pub external: bool,
    pub used_by: Vec<ImageUse>,
    /// The run this image came from, when it is one lmgw built.
    pub run_status: Option<BuildRunStatus>,
    /// The registry update check (§8, last bullet) for an image a class
    /// default or model override uses under a registry name (not
    /// `localhost/`). `None` for every other image, and before the first
    /// check.
    pub registry_update: Option<RegistryUpdate>,
}

/// A registry image's update state (§8): the digest the registry serves for
/// the tag now, against the digests podman recorded when the image was
/// pulled. Checked anonymously (the registry's bearer-token flow), by the same
/// schedule and **Check now** as the builds.
///
/// A multi-arch tag is an index: the registry answers with the index digest,
/// and podman's `RepoDigests` for such a pull normally carries it too. When it
/// carries only the platform manifest's digest instead, the index is read and
/// its members compared, so that case is not mistaken for an update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RegistryUpdate {
    /// When the registry was asked (RFC 3339).
    pub checked_at: String,
    /// The fully qualified reference that was checked
    /// (`ghcr.io/0xshug0/audio.cpp:full-cuda12`) — what **Pull update**
    /// (`container_image_pull`) pulls.
    pub reference: String,
    /// `sha256:…` the registry serves for the tag; `None` when it could not
    /// be asked (see `error`).
    pub remote_digest: Option<String>,
    /// The `sha256:…` digests podman recorded for this repository on the
    /// local image, right now.
    pub local_digests: Vec<String>,
    /// The registry serves something the local image is not.
    pub update_available: bool,
    /// Why the comparison could not be made: the registry did not answer, it
    /// wants credentials, it rate-limited the check, or podman recorded no
    /// digest to compare with (an image built or loaded here, not pulled).
    pub error: Option<String>,
}

/// `container_images`' `disk` field (§15, §9.1 Images tab footer):
/// `podman system df` totals plus orphaned build leftovers, shown for
/// information only — nothing is pruned from here (§11.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiskInfo {
    pub images_total: u64,
    pub reclaimable: u64,
    /// `/var/tmp/buildahNNN` dirs and `Storage` containers not yet cleaned
    /// up (§14.4).
    pub buildah_orphans: Vec<String>,
}

/// `container_images` response (§15): one entry per image ID, every tag of
/// it in `tags`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImagesResponse {
    pub images: Vec<ContainerImage>,
    pub disk: DiskInfo,
    /// The call asked for no disk footer (`disk: false`): `disk` is empty
    /// because it was not read, not because the store is.
    pub disk_skipped: bool,
}

/// `container_image_delete` response (§15).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImageDeleteResponse {
    pub removed: Vec<String>,
    /// A forced delete: the containers it stopped and removed first.
    pub removed_containers: Vec<String>,
    /// A forced delete: the class defaults and model overrides that still
    /// name the image — now a missing one.
    pub still_named_by: Vec<ImageUse>,
}

/// `container_image_tag` response (§15): the image's tags after the change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImageTagResponse {
    pub tags: Vec<String>,
}

/// One build's slot in a [`BuildUpdatesResponse`] (§15 `build_updates_check`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildUpdateEntry {
    pub build_id: i64,
    pub update: Option<UpdateStatus>,
}

/// `build_updates_check` response (§15, §8, WP6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildUpdatesResponse {
    pub updates: Vec<BuildUpdateEntry>,
}

/// The live `updates` frame on `/api/events` (§8 "the nav item shows the
/// count"): sent when a dashboard connects, and again whenever a check ends,
/// a run finishes, a build is saved, promoted or verified, or an image is
/// pulled — so the Backends badge is current without the page polling
/// anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpdatesSummary {
    /// Builds whose [`UpdateStatus::has_update`] is true.
    pub builds_with_updates: u32,
    /// Registry images in use whose [`RegistryUpdate::update_available`] is
    /// true.
    pub images_with_updates: u32,
    /// When the last full check (every build and image) ended; `None` before
    /// the first one.
    pub checked_at: Option<String>,
}

/// `container_image_pull` response (§8 **Pull update**): the background job
/// running `podman pull` (`JobRow.kind = "image_pull"`, `key =
/// "image:<reference>"`). A second pull of the same reference while one runs
/// answers with the running job's id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImagePullStarted {
    pub job_id: i64,
    pub image: String,
}

/// `JobRow.detail` for `kind = "image_pull"`: podman's latest output line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImagePullJobDetail {
    pub image: String,
    /// The image ID the reference named before the pull; `None` when it was
    /// not on this machine.
    pub old_id: Option<String>,
    pub last_line: String,
}

/// The result of a finished `image_pull` job (stored on its job row). A pull
/// replaces no running container: `used_by` is who still runs (or names) the
/// old image, for the page's "Recreate N running containers" offer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImagePullResult {
    pub image: String,
    pub old_id: Option<String>,
    pub new_id: String,
    /// `sha256:…` digests podman recorded for the reference before and after.
    pub old_digests: Vec<String>,
    pub new_digests: Vec<String>,
    /// The reference now names a different image than before.
    pub updated: bool,
    pub used_by: Vec<ImageUse>,
    /// podman's whole output, line by line.
    pub log: Vec<String>,
}

/// `container_image_pull_status` response: one `image_pull` job as it
/// stands — how the Backends page learns what a finished pull did (the jobs
/// feed carries only running jobs, and a job row's result is not on it).
/// `result` is set once the job is `done`; `error` once it `failed`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContainerImagePullStatus {
    pub job_id: i64,
    /// The reference being (or that was) pulled.
    pub image: String,
    /// `queued | running | done | failed | canceled`.
    pub status: String,
    /// The latest progress while it runs (podman's last line).
    pub detail: Option<ImagePullJobDetail>,
    pub result: Option<ImagePullResult>,
    pub error: Option<String>,
}

/// A `build_run` job's phase (§15 "Jobs feed", §5), shown in the job detail
/// and as the log's section headers.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BuildPhase {
    /// Waiting for the machine-wide build lock (§5 "Serialization") — shown
    /// as "waiting for <build>", named in `waiting_for`.
    #[default]
    Waiting,
    Resolve,
    Fetch,
    Assemble,
    Prepare,
    Build,
    Verify,
    Promote,
    Cleanup,
}

/// `JobRow.detail` for `kind = "build_run"`, `key = "build:<id>"` (§15
/// "Jobs feed").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildRunJobDetail {
    pub run_id: i64,
    pub build_id: i64,
    pub phase: BuildPhase,
    /// `"n/m"` progress within the current phase (podman's `STEP n/m:`,
    /// cmake/ninja's `[123/456]`, §5 step 5), when the phase has one. A
    /// multi-stage Dockerfile restarts podman's count in every stage.
    pub step: Option<String>,
    /// `"k/N"`: the podman stage `step` belongs to, when the line carried
    /// podman's `[k/N]` prefix. `None` for a single-stage build and outside
    /// the build phase.
    pub stage: Option<String>,
    pub percent: Option<u64>,
    pub last_line: String,
    /// Set only while `phase` is `waiting`: the build this run is queued
    /// behind.
    pub waiting_for: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_build_serializes_flat_with_ref_spelled_as_the_column() {
        let b = Build {
            id: 3,
            spec: BuildSpec {
                slug: "official-master".into(),
                git_ref: "master".into(),
                extras: vec![
                    BuildExtra::Pr {
                        number: 16391,
                        pin: None,
                    },
                    BuildExtra::Ref {
                        remote_url: "https://github.com/fork/llama.cpp".into(),
                        git_ref: "feature".into(),
                        pin: Some("a".repeat(40)),
                    },
                ],
                ..BuildSpec::default()
            },
            ..Build::default()
        };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["slug"], "official-master");
        assert_eq!(v["ref"], "master");
        assert_eq!(v["engine"], "llama");
        assert_eq!(v["backend"], "cuda");
        assert_eq!(v["ccache_max_size"], "10G");
        assert_eq!(v["extras"][0], json!({"kind": "pr", "number": 16391}));
        assert_eq!(v["extras"][1]["kind"], "ref");
        assert_eq!(v["extras"][1]["ref"], "feature");
        let back: Build = serde_json::from_value(v).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn statuses_round_trip_in_their_column_spelling() {
        for s in [
            BuildRunStatus::Running,
            BuildRunStatus::Succeeded,
            BuildRunStatus::Unverified,
            BuildRunStatus::Broken,
            BuildRunStatus::Failed,
            BuildRunStatus::Canceled,
            BuildRunStatus::UpToDate,
        ] {
            assert_eq!(BuildRunStatus::parse(s.as_str()), Some(s));
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()));
        }
        assert_eq!(BuildRunStatus::UpToDate.as_str(), "up_to_date");
    }

    #[test]
    fn the_moving_tag_is_the_engine_repo_and_the_slug() {
        assert_eq!(
            moving_tag(Engine::Llama, "ik-main"),
            "localhost/lmgw-llama-server:ik-main"
        );
        assert_eq!(moving_tag(Engine::Audio, "x"), "localhost/lmgw-audio-cpp:x");
        assert_eq!(moving_tag(Engine::Sdcpp, "x"), "localhost/lmgw-sd-server:x");
    }

    #[test]
    fn merge_outcome_matches_the_15_table_exactly() {
        for (v, s) in [
            (MergeOutcome::Merged, "merged"),
            (MergeOutcome::AlreadyInBase, "already_in_base"),
            (MergeOutcome::MergedUpstream, "merged_upstream"),
            (MergeOutcome::SquashApplied, "squash_applied"),
            (MergeOutcome::Conflict, "conflict"),
        ] {
            assert_eq!(serde_json::to_value(v).unwrap(), json!(s));
        }
    }

    #[test]
    fn image_use_kind_matches_the_15_table_exactly() {
        for (v, s) in [
            (ImageUseKind::ClassDefault, "class_default"),
            (ImageUseKind::ModelOverride, "model_override"),
            (ImageUseKind::RunningContainer, "running_container"),
            (ImageUseKind::StoppedContainer, "stopped_container"),
        ] {
            assert_eq!(serde_json::to_value(v).unwrap(), json!(s));
        }
    }

    #[test]
    fn build_phase_matches_the_15_table_exactly() {
        for (v, s) in [
            (BuildPhase::Waiting, "waiting"),
            (BuildPhase::Resolve, "resolve"),
            (BuildPhase::Fetch, "fetch"),
            (BuildPhase::Assemble, "assemble"),
            (BuildPhase::Prepare, "prepare"),
            (BuildPhase::Build, "build"),
            (BuildPhase::Verify, "verify"),
            (BuildPhase::Promote, "promote"),
            (BuildPhase::Cleanup, "cleanup"),
        ] {
            assert_eq!(serde_json::to_value(v).unwrap(), json!(s));
        }
    }

    #[test]
    fn build_set_args_default_when_fields_are_absent() {
        let args: BuildSetArgs = serde_json::from_value(json!({})).unwrap();
        assert_eq!(args.action, BuildSetAction::Create);
        assert_eq!(args.id, None);
        assert_eq!(args.spec, None);
        assert!(!args.delete_images);
    }

    #[test]
    fn build_view_round_trips() {
        let view = BuildView {
            moving_tag: "localhost/lmgw-dev-llama-server:official-master".into(),
            build: Build::default(),
            last_run: Some(BuildRun::default()),
            current_run: None,
            live_job_id: Some(42),
            used_by: vec![ImageUse {
                kind: ImageUseKind::ModelOverride,
                class: "chat".into(),
                model_id: Some("qwen".into()),
                container: None,
            }],
            update: Some(UpdateStatus {
                checked_at: "2026-09-26T00:00:00Z".into(),
                reasons: vec!["master +37 commits".into()],
                ref_moved: true,
                extras: vec![ExtraChange {
                    label: "PR #1234".into(),
                    change: "pushed".into(),
                }],
                errors: vec![],
            }),
        };
        let v = serde_json::to_value(&view).unwrap();
        let back: BuildView = serde_json::from_value(v).unwrap();
        assert_eq!(back, view);
    }

    #[test]
    fn resolved_preview_round_trips_with_build_arg_pairs() {
        let preview = ResolvedPreview {
            base_sha: "a".repeat(40),
            build_number: Some(11192),
            dockerfile: ".devops/cuda.Dockerfile".into(),
            target: "server".into(),
            profile: "official-cuda".into(),
            edits: vec![BuildEdit {
                name: "ccache-mount".into(),
                role: EditRole::Ccache,
                find: "RUN cmake".into(),
                replace: "RUN --mount=type=cache cmake".into(),
                required: true,
            }],
            build_args: vec![("GGML_CUDA_FA_ALL_QUANTS".into(), "ON".into())],
            moving_tag: "localhost/lmgw-llama-server:official-master".into(),
            immutable_tag: "localhost/lmgw-llama-server:official-master-abc1234-def567".into(),
            warnings: vec![],
            edit_outcomes: vec![EditApplied {
                index: 0,
                name: "ccache-mount".into(),
                role: EditRole::Ccache,
                required: true,
                applied: 1,
            }],
        };
        let v = serde_json::to_value(&preview).unwrap();
        assert_eq!(v["build_args"][0], json!(["GGML_CUDA_FA_ALL_QUANTS", "ON"]));
        let back: ResolvedPreview = serde_json::from_value(v).unwrap();
        assert_eq!(back, preview);
    }

    #[test]
    fn check_merge_report_round_trips() {
        let report = CheckMergeReport {
            base_sha: "b".repeat(40),
            ok: false,
            steps: vec![
                CheckMergeStep {
                    label: "PR #16391".into(),
                    outcome: MergeOutcome::Merged,
                    sha: "c".repeat(40),
                    files: vec![],
                    note: "merge commit c000000".into(),
                },
                CheckMergeStep {
                    label: "fork feature-x".into(),
                    outcome: MergeOutcome::Conflict,
                    sha: "d".repeat(40),
                    files: vec!["src/llama.cpp".into()],
                    note: "conflicts in src/llama.cpp".into(),
                },
            ],
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["steps"][1]["outcome"], "conflict");
        let back: CheckMergeReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn container_image_round_trips_with_provenance() {
        let image = ContainerImage {
            id: "e".repeat(64),
            tags: vec!["localhost/lmgw-llama-server:official-master".into()],
            engine: Some(Engine::Llama),
            backend: Some("cuda".into()),
            size: 8_500_000_000,
            created: "2026-09-26T00:00:00Z".into(),
            provenance: Some(ImageProvenance {
                build_id: 3,
                build_name: Some("official master".into()),
                run_id: 9,
                slug: "official-master".into(),
                repo: "https://github.com/ggml-org/llama.cpp".into(),
                git_ref: "master".into(),
                base: "f".repeat(40),
                extras: vec![ResolvedExtra {
                    extra: BuildExtra::Pr {
                        number: 16391,
                        pin: None,
                    },
                    sha: "1".repeat(40),
                }],
                instance: "ab12cd34".into(),
                other_instance: false,
            }),
            external: false,
            used_by: vec![],
            run_status: Some(BuildRunStatus::Succeeded),
            registry_update: None,
        };
        let v = serde_json::to_value(&image).unwrap();
        let back: ContainerImage = serde_json::from_value(v).unwrap();
        assert_eq!(back, image);
    }

    #[test]
    fn an_update_is_a_reason_not_an_error() {
        let mut u = UpdateStatus {
            checked_at: "2026-09-26T00:00:00Z".into(),
            errors: vec!["check failed: GitHub API rate limit reached".into()],
            ..UpdateStatus::default()
        };
        assert!(!u.has_update(), "a failed check is not an update");
        u.reasons.push("PR #1 pushed".into());
        assert!(u.has_update());
        // Older rows without `errors` still read.
        let old: UpdateStatus =
            serde_json::from_value(json!({"checked_at": "x", "reasons": [], "ref_moved": false}))
                .unwrap();
        assert!(old.errors.is_empty());
    }

    #[test]
    fn registry_update_and_pull_shapes_round_trip() {
        let image = ContainerImage {
            id: "a".repeat(64),
            tags: vec!["ghcr.io/0xshug0/audio.cpp:full-cuda12".into()],
            external: true,
            registry_update: Some(RegistryUpdate {
                checked_at: "2026-09-26T00:00:00Z".into(),
                reference: "ghcr.io/0xshug0/audio.cpp:full-cuda12".into(),
                remote_digest: Some(format!("sha256:{}", "b".repeat(64))),
                local_digests: vec![format!("sha256:{}", "c".repeat(64))],
                update_available: true,
                error: None,
            }),
            ..ContainerImage::default()
        };
        let v = serde_json::to_value(&image).unwrap();
        assert_eq!(v["registry_update"]["update_available"], true);
        let back: ContainerImage = serde_json::from_value(v).unwrap();
        assert_eq!(back, image);
        let old: ContainerImage = serde_json::from_value(json!({"id": "x"})).unwrap();
        assert!(old.registry_update.is_none());

        let summary = UpdatesSummary {
            builds_with_updates: 2,
            images_with_updates: 1,
            checked_at: Some("2026-09-26T06:00:00Z".into()),
        };
        let back: UpdatesSummary =
            serde_json::from_value(serde_json::to_value(&summary).unwrap()).unwrap();
        assert_eq!(back, summary);
        let result = ImagePullResult {
            image: "ghcr.io/x/y:z".into(),
            old_id: Some("1".repeat(64)),
            new_id: "2".repeat(64),
            updated: true,
            ..ImagePullResult::default()
        };
        let back: ImagePullResult =
            serde_json::from_value(serde_json::to_value(&result).unwrap()).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn container_images_args_ask_for_the_disk_footer_unless_told_not_to() {
        let absent: ContainerImagesArgs = serde_json::from_value(json!({})).unwrap();
        assert!(absent.disk, "absent keeps the old behaviour");
        assert_eq!(absent, ContainerImagesArgs::default());
        let picker: ContainerImagesArgs =
            serde_json::from_value(json!({"engine": "llama", "disk": false})).unwrap();
        assert_eq!((picker.engine, picker.disk), (Some(Engine::Llama), false));
        let old: ContainerImagesResponse = serde_json::from_value(json!({"images": []})).unwrap();
        assert!(!old.disk_skipped);
    }

    #[test]
    fn build_run_job_detail_round_trips() {
        let detail = BuildRunJobDetail {
            run_id: 5,
            build_id: 3,
            phase: BuildPhase::Build,
            step: Some("45/610".into()),
            stage: Some("2/6".into()),
            percent: Some(45),
            last_line: "[ 45%] Building CXX object ...".into(),
            waiting_for: None,
        };
        let v = serde_json::to_value(&detail).unwrap();
        assert_eq!(v["phase"], "build");
        assert_eq!(v["stage"], "2/6");
        // A detail written before `stage` existed still reads.
        let old: BuildRunJobDetail =
            serde_json::from_value(json!({"run_id": 5, "phase": "build", "step": "1/7"})).unwrap();
        assert_eq!(old.stage, None);
        let back: BuildRunJobDetail = serde_json::from_value(v).unwrap();
        assert_eq!(back, detail);
    }

    /// A typo'd argument used to deserialize to its default and the op ran
    /// with it; now it is refused under its name.
    #[test]
    fn op_args_refuse_unknown_fields() {
        let e = serde_json::from_value::<BuildRunArgs>(json!({"id": 3, "rebuidl": true}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("rebuidl"), "{e}");
        assert!(serde_json::from_value::<ContainerImagesArgs>(json!({"dsik": false})).is_err());
        assert!(serde_json::from_value::<BuildUpdatesCheckArgs>(json!({"ids": 1})).is_err());
        // Absent fields still default.
        let a: BuildRunArgs = serde_json::from_value(json!({"id": 3})).unwrap();
        assert!(!a.rebuild);
    }

    #[test]
    fn build_run_log_args_carry_tail_only_when_set() {
        let offset = BuildRunLogArgs {
            run_id: 5,
            offset: 10,
            tail: None,
        };
        assert_eq!(
            serde_json::to_value(offset).unwrap(),
            json!({"run_id": 5, "offset": 10})
        );
        let tail: BuildRunLogArgs =
            serde_json::from_value(json!({"run_id": 5, "tail": 200})).unwrap();
        assert_eq!((tail.offset, tail.tail), (0, Some(200)));
    }
}
