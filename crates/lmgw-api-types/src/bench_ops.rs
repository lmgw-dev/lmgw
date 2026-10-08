//! The benchmark ops' arguments and answers (benchmark design §8.1): what the
//! Benchmarks page (§8.2) and the `lmgw__bench_*` tools send to
//! `POST /api/op/bench_*`, and what comes back.
//!
//! Beside [`crate::bench`] rather than in it only for file size: the stored
//! shapes (the `bench_runs` JSON columns) live there, the op envelopes here,
//! and [`BenchRun`] is the one place both meet.
//!
//! The overrides (§3.5) are **flat** on [`BenchArgs`], so the same field
//! names reach the op from the dashboard and from an MCP tool's scalar
//! arguments. The one difference is `phases`: an array here, a
//! comma-separated string on the tools.

use serde::{Deserialize, Serialize};

use crate::bench::{
    BuildIdentity, GpuIdentity, ModelIdentity, Phase, PointPlan, ProbeKind, ProbeOutcome,
    ProbeReport, RunResults, SuiteParams, Timeline, DEFAULT_REPETITIONS,
};

/// `JobRow.kind` of a run (`jobs` feed filter).
pub const JOB_KIND: &str = "benchmark";

/// `JobRow.key` of every run: one key for all of them, so the jobs table's
/// `(kind, key)` guard is "one run at a time" (§3.1).
pub const JOB_KEY: &str = "gpu";

/// The regression threshold when a view names none. Always
/// echoed in the answer, never applied silently.
pub const DEFAULT_THRESHOLD_PCT: f64 = 5.0;

/// What a run changes about the row's container, and only about the
/// bench container. `None` (and `false`, `0`) is "as the row says".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchOverrides {
    /// A ladder row's rung, 0 = base.
    pub rung: u32,
    /// Another image, e.g. a Backends build (comparing two builds is two runs
    /// of one row with two images).
    pub image: Option<String>,
    pub ctx_size: Option<i64>,
    pub parallel: Option<i64>,
    pub ubatch_size: Option<i64>,
    pub batch_size: Option<i64>,
    pub cache_type_k: Option<String>,
    pub cache_type_v: Option<String>,
    /// `auto` / `on` / `off`.
    pub flash_attn: Option<String>,
    pub kv_unified: Option<bool>,
    pub n_gpu_layers: Option<i64>,
    /// Run without the row's speculative drafter.
    pub no_draft: bool,
}

impl BenchOverrides {
    /// True when the run renders exactly the row (at `rung`).
    pub fn changes_nothing(&self) -> bool {
        self.image.is_none()
            && self.ctx_size.is_none()
            && self.parallel.is_none()
            && self.ubatch_size.is_none()
            && self.batch_size.is_none()
            && self.cache_type_k.is_none()
            && self.cache_type_v.is_none()
            && self.flash_attn.is_none()
            && self.kv_unified.is_none()
            && self.n_gpu_layers.is_none()
            && !self.no_draft
    }
}

/// `bench_plan` and `bench_start`: the row, the flat overrides, the
/// phases and repetitions. `notes` is stored by `bench_start` and ignored by
/// `bench_plan`, so the dashboard's modal sends one shape to both.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchArgs {
    /// A local chat row's `model_id`.
    pub model_id: String,
    pub rung: u32,
    pub image: Option<String>,
    pub ctx_size: Option<i64>,
    pub parallel: Option<i64>,
    pub ubatch_size: Option<i64>,
    pub batch_size: Option<i64>,
    pub cache_type_k: Option<String>,
    pub cache_type_v: Option<String>,
    pub flash_attn: Option<String>,
    pub kv_unified: Option<bool>,
    pub n_gpu_layers: Option<i64>,
    pub no_draft: bool,
    /// The phases to run, in any order; `load` always runs, and an empty list
    /// runs every phase.
    pub phases: Vec<Phase>,
    /// Repetitions per point; absent is 3.
    pub repetitions: Option<u32>,
    pub notes: String,
}

impl BenchArgs {
    pub fn overrides(&self) -> BenchOverrides {
        BenchOverrides {
            rung: self.rung,
            image: self.image.clone().filter(|i| !i.trim().is_empty()),
            ctx_size: self.ctx_size,
            parallel: self.parallel,
            ubatch_size: self.ubatch_size,
            batch_size: self.batch_size,
            cache_type_k: self.cache_type_k.clone().filter(|v| !v.trim().is_empty()),
            cache_type_v: self.cache_type_v.clone().filter(|v| !v.trim().is_empty()),
            flash_attn: self.flash_attn.clone().filter(|v| !v.trim().is_empty()),
            kv_unified: self.kv_unified,
            n_gpu_layers: self.n_gpu_layers,
            no_draft: self.no_draft,
        }
    }

    /// Suite v1 with this request's two choices.
    pub fn params(&self) -> SuiteParams {
        let phases = if self.phases.is_empty() {
            Phase::ALL.to_vec()
        } else {
            self.phases.clone()
        };
        SuiteParams::v1(self.repetitions.unwrap_or(DEFAULT_REPETITIONS), phases)
    }
}

/// The `settings` of a run: what the bench container runs,
/// after the overrides.
///
/// **The settings hash covers `params`, `args` and `extra_run_args` only**: the image is the build
/// identity and may differ between
/// comparable runs (that is the point of comparing builds), the weights file
/// is the model identity, and `overrides` only says how these values came
/// about — the same effective flags reached with or without an override are
/// the same settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchSettings {
    /// The resolved image the run starts (override, row, or class default).
    pub image: String,
    /// The weights file, relative to the chat models dir (the rung's own for
    /// a ladder rung).
    pub gguf_path: String,
    pub rung: u32,
    /// The effective `LlamaParams`, as the row stores them.
    pub params: serde_json::Value,
    /// The row's freeform llama-server flags (minus the drafter's, with
    /// `no_draft`).
    pub args: Vec<String>,
    /// `podman run` flags (GPU devices, …): the row's, or its class's.
    pub extra_run_args: Vec<String>,
    pub overrides: BenchOverrides,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BenchStatus {
    #[default]
    Running,
    Done,
    Failed,
    Canceled,
    /// Ended by something other than a cancel request — the GPU hold
    /// switching on (`status_reason: "hold"`).
    Aborted,
    /// Found `running` at boot: lmgw went away mid-run.
    Interrupted,
}

impl BenchStatus {
    pub const ALL: [BenchStatus; 6] = [
        Self::Running,
        Self::Done,
        Self::Failed,
        Self::Canceled,
        Self::Aborted,
        Self::Interrupted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Aborted => "aborted",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// One container a start stops — what the confirmation lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchStop {
    /// `chat` | `aux` | `audio` | `image`.
    pub class: String,
    pub model_id: String,
    pub container_name: String,
    /// The registry's state: `starting` | `ready` | `stopping`.
    pub state: String,
    /// Serving lmgw requests right now, or still starting: the run waits for
    /// it to drain rather than cutting it off. A client generating on the
    /// container's own port is only seen at start time (`/slots`).
    pub busy: bool,
    pub in_flight: usize,
}

/// Why a run cannot start now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BlockedReason {
    /// The GPU hold is on: a benchmark is lmgw using VRAM.
    #[default]
    Hold,
    /// Another run holds the card; `run_id` names it.
    RunGoing,
    /// No chat row has this id.
    RowMissing,
    RowDisabled,
    /// The id names an aux, audio or image row.
    NotChat,
    /// `rung` is past the row's top rung.
    RungOutOfRange,
    /// The image is not on this machine: `podman run` would pull it and the
    /// load phase would time the download.
    ImageMissing,
    /// The chat models dir is not configured, or the weights are not in it.
    NoWeights,
    /// lmgw has just started and has not yet adopted the containers a
    /// previous lmgw left running: the drain could not see them yet.
    Booting,
    /// The `image` override is not an image reference: it starts with `-`
    /// (podman would read it as a flag) or holds whitespace.
    InvalidImage,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchBlocked {
    pub reason: BlockedReason,
    /// The sentence to show.
    pub message: String,
    /// The running run, for the `run_going` reason.
    pub run_id: Option<i64>,
}

/// A probe the plan would run, or why it would not.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlannedProbe {
    pub probe: ProbeKind,
    /// `None`: it runs. Judged on the row's capabilities; the live server's
    /// `chat_template_caps` may still skip `tool_call` at run time.
    pub skip: Option<String>,
}

/// `bench_plan`'s answer: everything the confirmation shows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchPlan {
    pub model_id: String,
    pub rung: u32,
    /// The row's rungs, base included (1 for a row without a ladder).
    pub rungs: u32,
    pub settings: BenchSettings,
    pub settings_hash: String,
    /// `podman run …` as it would be started, with the port it would get
    /// shown as `<port>`.
    pub command_line: String,
    pub model: ModelIdentity,
    pub build: BuildIdentity,
    pub params: SuiteParams,
    /// Derived from the row's numbers (`provisional: true`); a run derives
    /// them again from the live server.
    pub points: PointPlan,
    /// Progress steps the plan takes (`JobRow.total`), from the provisional
    /// points.
    pub total_steps: u64,
    pub probes: Vec<PlannedProbe>,
    /// Every lmgw container on the card, which a start stops (all classes).
    pub stops: Vec<BenchStop>,
    /// Why it cannot start now; `None` means `bench_start` would start it.
    pub blocked: Option<BenchBlocked>,
    /// Things worth knowing that do not block (a projector the row cannot
    /// read, …).
    pub warnings: Vec<String>,
}

/// `bench_start`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchStarted {
    pub run_id: i64,
    pub job_id: i64,
    /// What the run is stopping (as `bench_plan` listed it).
    pub stops: Vec<BenchStop>,
    pub message: String,
}

/// `bench_runs`: run summaries, newest first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchRunsArgs {
    /// Only this row's runs.
    pub model_id: Option<String>,
    /// Page size, at least 1; absent returns every run.
    pub limit: Option<u32>,
    /// Exclusive run-id cursor: runs older than this one.
    pub before: Option<i64>,
    /// The regression badge's threshold; absent is 5 (percent).
    pub threshold_pct: Option<f64>,
}

/// The headline numbers of one run, each `None` when the run did not
/// measure it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Headline {
    /// Prefill tok/s at 2048 tokens, or the nearest measured length.
    pub prefill_tok_s: Option<f64>,
    /// The length it was measured at.
    pub prefill_tokens: Option<u64>,
    /// TTFT at that same length.
    pub ttft_ms: Option<f64>,
    /// Decode tok/s at depth 64.
    pub decode_tok_s: Option<f64>,
    /// Decode tok/s at the deepest point, *D_max*.
    pub decode_deep_tok_s: Option<f64>,
    pub decode_deep_depth: Option<u64>,
    /// Aggregate tok/s at *N_slots* streams.
    pub aggregate_tok_s: Option<f64>,
    pub aggregate_streams: Option<u32>,
    /// Decode tokens per joule at depth 64.
    pub decode_tokens_per_joule: Option<f64>,
    pub load_ms: Option<u64>,
    /// VRAM the load took (after load minus the baseline).
    pub load_vram_bytes: Option<u64>,
    /// The mixed phase's median stall.
    pub stall_ms: Option<f64>,
}

/// One row of the runs table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchRunSummary {
    pub id: i64,
    pub job_id: Option<i64>,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub status: BenchStatus,
    pub status_reason: Option<String>,
    pub error: Option<String>,
    /// Every selected phase finished without an error.
    pub complete: bool,
    pub model_id: String,
    pub quant: Option<String>,
    pub rung: u32,
    pub image_ref: String,
    /// `dev.lmgw.slug`, `org.opencontainers.image.version`, the commit and
    /// `/props` `build_info` — whichever the build has.
    pub engine_slug: Option<String>,
    pub version: Option<String>,
    pub commit: Option<String>,
    pub build_info: Option<String>,
    pub gpu_name: Option<String>,
    pub throttled: bool,
    pub suite_version: u32,
    pub repetitions: u32,
    pub phases: Vec<Phase>,
    pub headline: Headline,
    /// Probes that passed, and probes that gave a verdict (`pass`, `fail`,
    /// `error` — not `info` or `skipped`).
    pub probes_passed: u32,
    pub probes_judged: u32,
    pub notes: String,
    /// The previous comparable run, when there is one.
    pub previous_id: Option<i64>,
    /// Against it, at the answer's `threshold_pct`.
    pub regressions: Option<u32>,
    pub improvements: Option<u32>,
    /// The stored columns that did not decode. Such a
    /// run is listed with what could be read, and compared with nothing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchRunsResponse {
    pub runs: Vec<BenchRunSummary>,
    /// Older runs exist beyond this page.
    pub more: bool,
    pub threshold_pct: f64,
}

/// `bench_run`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchRunArgs {
    pub id: i64,
    /// Absent is 5 (percent); echoed in the answer.
    pub threshold_pct: Option<f64>,
    /// Include the 500 ms timeline (`run.timeline.samples`). Absent is
    /// `true`; the MCP tool sends `false` unless asked, since it is the
    /// bulk of a run.
    pub timeline: Option<bool>,
}

/// One stored run, whole: every column, the JSON ones typed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchRun {
    pub id: i64,
    pub job_id: Option<i64>,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub status: BenchStatus,
    /// Why it ended the way it did, in one word where there is one
    /// (`hold`, `canceled`, `shutdown`).
    pub status_reason: Option<String>,
    pub error: Option<String>,
    pub model: ModelIdentity,
    pub image_ref: String,
    pub image_id: Option<String>,
    pub build: BuildIdentity,
    pub settings: BenchSettings,
    pub settings_hash: String,
    pub command_line: String,
    pub gpu: GpuIdentity,
    pub suite_version: u32,
    pub params: SuiteParams,
    pub results: RunResults,
    pub probes: ProbeReport,
    pub timeline: Timeline,
    pub notes: String,
    /// The JSON columns that did not decode, each as "<column>: <why>". Each reads as its empty
    /// default here, so the run
    /// is comparable with nothing ):
    /// read as empty, it would compare as "measured nothing".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
}

/// How one metric moved (by the regression rule).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Verdict {
    Regression,
    Improvement,
    /// Within the band: max(threshold, noise).
    #[default]
    Same,
    /// One of the two runs did not measure it.
    Missing,
    /// Both runs measured it, but at different points — another prompt
    /// length, depth or stream count, as when one run stopped early. The delta is kept as
    /// information; it is no verdict, so
    /// it counts neither as a regression nor as an improvement.
    NotSamePoint,
}

/// One headline metric of a comparison.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MetricDelta {
    /// Stable key: `prefill_tok_s`, `ttft_ms`, `decode_tok_s`,
    /// `decode_deep_tok_s`, `aggregate_tok_s`, `decode_tokens_per_joule`,
    /// `load_ms`, `load_vram_bytes`, `stall_ms`.
    pub metric: String,
    pub label: String,
    pub unit: String,
    pub lower_is_better: bool,
    /// The previous run's value (median), and this run's.
    pub old: Option<f64>,
    pub new: Option<f64>,
    /// (new − old) / old, in percent.
    pub delta_pct: Option<f64>,
    /// The larger relative spread of the two points, in percent.
    pub noise_pct: f64,
    /// max(threshold, noise): the band a delta has to leave.
    pub band_pct: f64,
    pub verdict: Verdict,
    /// E.g. the two runs measured different prompt lengths (the verdict is
    /// then `not_same_point`).
    pub note: Option<String>,
}

/// One probe's outcome in both runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProbeChange {
    pub probe: ProbeKind,
    pub old: Option<ProbeOutcome>,
    pub new: Option<ProbeOutcome>,
    /// A pass that became a fail or an error is a regression; the reverse an
    /// improvement.
    pub verdict: Verdict,
}

/// A run against another: the previous comparable run for `bench_run`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchComparison {
    pub base_run_id: i64,
    pub base_created_at: String,
    /// Same model file, settings hash, suite version and GPU.
    pub comparable: bool,
    /// Why not, when not.
    pub not_comparable: Vec<String>,
    pub threshold_pct: f64,
    pub metrics: Vec<MetricDelta>,
    pub probes: Vec<ProbeChange>,
    /// Metrics and probes flagged each way (`not_same_point` and
    /// `missing` count for neither).
    pub regressions: u32,
    pub improvements: u32,
}

/// `bench_run`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchRunDetail {
    pub run: BenchRun,
    pub headline: Headline,
    /// Against the previous comparable run; `None` when there is none.
    pub comparison: Option<BenchComparison>,
    pub threshold_pct: f64,
    /// The job still running this run, for the live jobs feed.
    pub live_job_id: Option<i64>,
}

/// `bench_cancel`: the running run; `run_id` only has to match it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchCancelArgs {
    pub run_id: Option<i64>,
}

/// `bench_run_set`: the notes on a run (dashboard only).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchRunSetArgs {
    pub id: i64,
    pub notes: String,
}

/// `bench_delete`: a finished run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchDeleteArgs {
    pub id: i64,
}

/// `bench_cancel`, `bench_run_set` and `bench_delete`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchDone {
    pub ok: bool,
    pub run_id: i64,
    pub message: String,
}

/// `JobRow.detail` of a `benchmark` job: which run it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BenchJobDetail {
    pub run_id: i64,
    pub model_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips() {
        for s in BenchStatus::ALL {
            assert_eq!(BenchStatus::parse(s.as_str()), Some(s));
            assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        }
        assert_eq!(BenchStatus::parse("nope"), None);
    }

    #[test]
    fn args_split_into_overrides_and_params() {
        let a: BenchArgs = serde_json::from_str(
            r#"{"model_id":"m","rung":1,"image":"  ","ctx_size":8192,"phases":["decode"],
                "repetitions":2}"#,
        )
        .unwrap();
        let o = a.overrides();
        assert_eq!(o.rung, 1);
        assert_eq!(o.image, None, "a blank image is no override");
        assert_eq!(o.ctx_size, Some(8192));
        assert!(!o.changes_nothing());
        let p = a.params();
        assert_eq!(p.phases, vec![Phase::Load, Phase::Decode]);
        assert_eq!(p.repetitions, 2);
        let all = BenchArgs::default().params();
        assert_eq!(all.phases, Phase::ALL.to_vec());
        assert_eq!(all.repetitions, DEFAULT_REPETITIONS);
        assert!(BenchArgs::default().overrides().changes_nothing());
    }

    #[test]
    fn args_refuse_an_unknown_field() {
        let e = serde_json::from_str::<BenchArgs>(r#"{"model_id":"m","ctx":1}"#).unwrap_err();
        assert!(e.to_string().contains("ctx"), "{e}");
    }
}
