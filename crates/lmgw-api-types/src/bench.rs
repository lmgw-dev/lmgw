//! Benchmark runs (docs/design/2026-09-29-benchmark-design.md).
//!
//! The stable contract between the engine (`lmgw_core::bench`), the store
//! (`bench_runs`' JSON columns, design §7), the ops (§8.1) and the Benchmarks
//! page (§8.2). The engine fills these shapes, the store keeps them verbatim,
//! the page reads them back — so none of the three can disagree about what a
//! field means.
//!
//! Units are in the field names wherever a number has one (`_ms`, `_bytes`,
//! `_w`, `_c`, `tok_s` = tokens per second). A figure the engine could not
//! measure is `None`, never zero: "no telemetry" and "measured nothing" are
//! different answers.
//!
//! Every struct is `#[serde(default)]`, so a row written by an older build
//! still reads — a missing field comes back as its default instead of failing
//! the whole run.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The suite's version (§4). Changing any constant below, or any method in
/// the engine, bumps it; runs of different versions are never compared.
pub const SUITE_VERSION: u32 = 1;

/// Repetitions per point unless the run says otherwise (§4.2).
pub const DEFAULT_REPETITIONS: u32 = 3;

/// *G*: tokens generated per decode request (§4.2).
pub const GENERATE_TOKENS: u32 = 256;

/// The prompt each concurrent and each mixed decoding stream starts from
/// (§4.2).
pub const STREAM_PROMPT_TOKENS: u64 = 256;

/// The mixed phase injects min(this, *P_max*) tokens (§4.2).
pub const MIXED_INJECT_MAX_TOKENS: u64 = 8192;

/// Steady decode before the mixed phase injects its prompt, and the window
/// its "before" rate is measured over (§4.3).
pub const MIXED_STEADY_MS: u64 = 2000;

/// The needle probe's prompt is *P_max* minus this: room for the question,
/// the chat template and the answer (§5).
pub const NEEDLE_MARGIN_TOKENS: u64 = 512;

/// The unmeasured warm-up request after load, before the first measured
/// phase: a prompt of this many tokens (or less on a tiny context) …
pub const WARMUP_PROMPT_TOKENS: u64 = 512;

/// … generating this many. A fresh server's first requests are slow (CUDA
/// module loading, buffer growth); without the warm-up the first measured
/// point pays for it, and it pays more when the probes are deselected —
/// live, prefill at 512 read 12k/18.7k tok/s cold against 26k warm.
pub const WARMUP_GENERATE_TOKENS: u32 = 16;

/// The sampler's tick, for peaks and energy (§4.3).
pub const SAMPLE_INTERVAL_MS: u64 = 100;

/// The stored timeline's spacing (§4.3).
pub const TIMELINE_INTERVAL_MS: u64 = 500;

/// NVML clock-event ("throttle") reason bits, with whether each one means
/// the card was actually held back. `GPU_IDLE`, the applications-clocks
/// setting, sync boost and the display clock are states, not throttling;
/// the power cap and the slowdowns are. `(bit, name, throttles)`.
pub const CLOCK_EVENT_REASONS: &[(u64, &str, bool)] = &[
    (0x1, "gpu_idle", false),
    (0x2, "applications_clocks_setting", false),
    (0x4, "sw_power_cap", true),
    (0x8, "hw_slowdown", true),
    (0x10, "sync_boost", false),
    (0x20, "sw_thermal_slowdown", true),
    (0x40, "hw_thermal_slowdown", true),
    (0x80, "hw_power_brake_slowdown", true),
    (0x100, "display_clock_setting", false),
];

/// The bits of [`CLOCK_EVENT_REASONS`] that count as throttling.
pub const THROTTLE_MASK: u64 = 0x4 | 0x8 | 0x20 | 0x40 | 0x80;

/// One phase of a run, in suite order (§4.4).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Phase {
    /// `podman run` to a healthy server (the integration's, not the engine's).
    #[default]
    Load,
    Probes,
    Prefill,
    Decode,
    Concurrent,
    Mixed,
}

impl Phase {
    /// Suite order: load → probes → prefill → decode → concurrent → mixed.
    pub const ALL: [Phase; 6] = [
        Phase::Load,
        Phase::Probes,
        Phase::Prefill,
        Phase::Decode,
        Phase::Concurrent,
        Phase::Mixed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Load => "load",
            Phase::Probes => "probes",
            Phase::Prefill => "prefill",
            Phase::Decode => "decode",
            Phase::Concurrent => "concurrent",
            Phase::Mixed => "mixed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// The MCP tools' `phases` argument: a comma-separated list (§8.1).
    /// Answers the phases in suite order, deduplicated, with `load` always
    /// present (§4.4: load always runs). An empty string selects every phase.
    /// An unknown name is an error naming the valid ones.
    pub fn parse_list(csv: &str) -> Result<Vec<Phase>, String> {
        let mut out = vec![Phase::Load];
        let names: Vec<&str> = csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if names.is_empty() {
            return Ok(Self::ALL.to_vec());
        }
        for name in names {
            let p = Self::parse(name).ok_or_else(|| {
                format!(
                    "unknown phase '{name}'; the phases are {}",
                    Self::ALL.map(Phase::as_str).join(", ")
                )
            })?;
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out.sort();
        Ok(out)
    }
}

/// The sampling every request of a run uses (§4.2): pinned, so builds compare
/// and speculative decoding's acceptance does not wander with the row's own
/// sampler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Sampling {
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: u32,
    pub seed: u64,
}

impl Default for Sampling {
    /// Suite v1: 0.7 / 0.9 / 40 / seed 1234.
    fn default() -> Self {
        Self {
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            seed: 1234,
        }
    }
}

/// What a run was asked to do (§7 `params`). The suite constants are stored
/// beside the owner's choices, so a stored run says exactly what it measured
/// without anyone having to know which suite version meant what.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SuiteParams {
    pub suite_version: u32,
    /// Repetitions of every point (and of the mixed phase).
    pub repetitions: u32,
    /// The selected phases, in suite order; `load` is always among them.
    pub phases: Vec<Phase>,
    /// *G*.
    pub generate_tokens: u32,
    pub sampling: Sampling,
    pub stream_prompt_tokens: u64,
    pub mixed_inject_max_tokens: u64,
    pub mixed_steady_ms: u64,
    pub needle_margin_tokens: u64,
    pub warmup_prompt_tokens: u64,
    pub warmup_generate_tokens: u32,
}

impl Default for SuiteParams {
    fn default() -> Self {
        Self::v1(DEFAULT_REPETITIONS, Phase::ALL.to_vec())
    }
}

impl SuiteParams {
    /// Suite v1 with the owner's two choices. `load` is added when missing and
    /// the phases are put in suite order; a repetition count of 0 is read as 1.
    pub fn v1(repetitions: u32, phases: Vec<Phase>) -> Self {
        let mut phases = phases;
        if !phases.contains(&Phase::Load) {
            phases.push(Phase::Load);
        }
        phases.sort();
        phases.dedup();
        Self {
            suite_version: SUITE_VERSION,
            repetitions: repetitions.max(1),
            phases,
            generate_tokens: GENERATE_TOKENS,
            sampling: Sampling::default(),
            stream_prompt_tokens: STREAM_PROMPT_TOKENS,
            mixed_inject_max_tokens: MIXED_INJECT_MAX_TOKENS,
            mixed_steady_ms: MIXED_STEADY_MS,
            needle_margin_tokens: NEEDLE_MARGIN_TOKENS,
            warmup_prompt_tokens: WARMUP_PROMPT_TOKENS,
            warmup_generate_tokens: WARMUP_GENERATE_TOKENS,
        }
    }

    pub fn has(&self, phase: Phase) -> bool {
        self.phases.contains(&phase)
    }
}

/// The points a run measures (§4.2), derived from the live server's per-slot
/// context and slot count — or, in `bench_plan`, provisionally from the row's
/// own numbers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PointPlan {
    /// *S*.
    pub per_slot_ctx: u64,
    /// *N_slots*.
    pub n_slots: u32,
    /// *G*.
    pub generate_tokens: u32,
    pub repetitions: u32,
    /// Prompt lengths *P*, ascending; the last one is *P_max* = *S* − 2.
    pub prefill: Vec<u64>,
    /// Context depths *D*, ascending; the last one is *D_max* = *S* − *G* − 1.
    pub decode: Vec<u64>,
    /// Stream counts *N*, ascending; the last one is *N_slots*.
    pub concurrent: Vec<u32>,
    /// `None` when the server has a single slot, or its context cannot hold
    /// a decoding stream.
    pub mixed: Option<MixedPlan>,
    /// The needle probe's prompt length, *P_max* − 512; `None` when the
    /// context is too small to hold one.
    pub needle_tokens: Option<u64>,
    /// True when derived from the row's configuration rather than read from a
    /// running server (`bench_plan`).
    pub provisional: bool,
    /// Why a phase has fewer points than usual, or none at all.
    pub notes: Vec<String>,
}

impl PointPlan {
    /// *P_max*, when there is any prefill point.
    pub fn p_max(&self) -> Option<u64> {
        self.prefill.last().copied()
    }
}

/// The mixed phase's shape (§4.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MixedPlan {
    /// *N_slots* − 1 decoding streams.
    pub streams: u32,
    /// Their prompt length.
    pub stream_prompt_tokens: u64,
    /// Their `n_predict`: as much as the slot holds, so none runs out before
    /// the injected prompt's first token.
    pub stream_predict: u64,
    /// *P_inj* = min(8192, *P_max*).
    pub inject_tokens: u64,
}

/// Median, min and max of one metric over a point's repetitions, and the raw
/// values in repetition order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Stat {
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub values: Vec<f64>,
}

/// Where a joule figure came from (§2.3, §4.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum EnergySource {
    /// The driver's total-energy counter, read at both ends of the window
    /// (NVML `nvmlDeviceGetTotalEnergyConsumption`).
    #[default]
    Counter,
    /// Power samples integrated over the window (trapezoids), because the
    /// counter is not available.
    Integrated,
}

/// The energy of a measured window: a point's, summed over its repetitions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Energy {
    /// The whole card, idle draw included (§4.3).
    pub joules: f64,
    /// Length of the measured windows, summed.
    pub seconds: f64,
    pub avg_w: f64,
    /// The tokens those windows produced (prefill: prompt tokens; decode:
    /// generated tokens).
    pub tokens: u64,
    /// `tokens / joules`; `None` when the windows measured 0 J.
    pub tokens_per_joule: Option<f64>,
    /// Tokens per joule of each window on its own (one per repetition, or
    /// per request for prefill): the spread the comparison reads as this
    /// figure's noise. A half-second decode window right after an idle
    /// moment is measured while the card's clocks ramp up — live, decode at
    /// depth 64 read 2.47 and 3.01 tok/J in two runs of identical settings.
    /// Windows the counter did not move over are left out.
    pub tokens_per_joule_each: Stat,
    pub source: EnergySource,
}

/// Speculative decoding's draft statistics, summed over a point's requests
/// (§4.3). Present only when the server reported them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DraftStats {
    pub drafted: u64,
    pub accepted: u64,
    /// `accepted / drafted`, `None` when nothing was drafted.
    pub acceptance: Option<f64>,
}

/// One prefill point (§4.3): `n_predict: 1`, `cache_prompt: false`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PrefillPoint {
    /// *P*.
    pub prompt_tokens: u64,
    /// Server-side `timings.prompt_per_second`.
    pub prompt_tok_s: Stat,
    /// Server-side `timings.prompt_ms`.
    pub prompt_ms: Stat,
    /// Client-side: request sent → first streamed token.
    pub ttft_ms: Stat,
    /// `timings.prompt_n`: what the server actually evaluated. Equal to *P*
    /// when the KV cache stayed out of it.
    pub evaluated_tokens: Stat,
    /// Over the TTFT windows; tokens are the evaluated prompt tokens.
    pub energy: Option<Energy>,
    /// Why `energy` is `None` although the card reports power: a window
    /// shorter than the GPU sampler resolves (two of its ticks, 200 ms at
    /// the 100 ms tick — §13 decision 56), whose figure would have been the
    /// ticks' average power times its length rather than a measurement.
    /// `None` otherwise.
    pub energy_unmeasured: Option<String>,
}

/// One decode point (§4.3): `n_predict: G`, `ignore_eos`, `cache_prompt: true`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecodePoint {
    /// *D*: the prompt length the generation starts from.
    pub depth: u64,
    /// Server-side `timings.predicted_per_second`.
    pub tok_s: Stat,
    /// `timings.predicted_n`.
    pub generated: Stat,
    /// `timings.prompt_n`: the first repetition primes the cache (≈ *D*), the
    /// later ones reuse it (a handful).
    pub evaluated_tokens: Stat,
    pub draft: Option<DraftStats>,
    /// Distinct token ids over generated ones, per repetition: near 1 for
    /// prose, low for a degenerate loop that `ignore_eos` ran into — and a
    /// loop is what a drafter predicts perfectly, so a draft acceptance
    /// next to a low ratio flatters it (§13 decision 58). Only where the
    /// engine streams token ids (official llama.cpp, not ik).
    pub distinct_token_ratio: Option<Stat>,
    /// Over first token → last token of each repetition, so the priming
    /// prefill stays out; tokens are those generated after the first.
    pub energy: Option<Energy>,
    /// Why `energy` is `None` although the card reports power: a window
    /// shorter than the GPU sampler resolves (two of its ticks, 200 ms at
    /// the 100 ms tick — §13 decision 56), whose figure would have been the
    /// ticks' average power times its length rather than a measurement.
    /// `None` otherwise.
    pub energy_unmeasured: Option<String>,
}

/// One concurrent point (§4.3): *N* streams released together.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConcurrentPoint {
    /// *N*.
    pub streams: u32,
    /// Σ `predicted_n` / (last stream's end − first stream's first token).
    pub aggregate_tok_s: Stat,
    /// The median of the streams' `predicted_per_second`, per repetition.
    pub per_stream_tok_s: Stat,
    pub draft: Option<DraftStats>,
    /// Over the aggregate's window; tokens are Σ `predicted_n`.
    pub energy: Option<Energy>,
    /// Why `energy` is `None` although the card reports power: a window
    /// shorter than the GPU sampler resolves (two of its ticks, 200 ms at
    /// the 100 ms tick — §13 decision 56), whose figure would have been the
    /// ticks' average power times its length rather than a measurement.
    /// `None` otherwise.
    pub energy_unmeasured: Option<String>,
}

/// The mixed phase (§4.3): what an arriving long prompt costs streams that
/// are already decoding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MixedResult {
    /// Decoding streams (*N_slots* − 1).
    pub streams: u32,
    /// *P_inj*.
    pub inject_tokens: u64,
    /// Per repetition: the median over the streams of each one's rate in the
    /// steady window before the injection.
    pub before_tok_s: Stat,
    /// Per repetition: the median over the streams of each one's rate from
    /// the injection to the injected request's first token.
    pub during_tok_s: Stat,
    /// Per repetition: the longest inter-token gap of any stream across that
    /// window — the stall.
    pub stall_ms: Stat,
    /// The injected request's TTFT.
    pub inject_ttft_ms: Stat,
    /// The prefill phase's median TTFT at the same prompt length, when it
    /// measured that length.
    pub solo_ttft_ms: Option<f64>,
    /// Anything that makes a repetition's numbers partial (a stream that
    /// ended before the injected first token, …).
    pub notes: Vec<String>,
}

/// The load phase (§4.3), measured by the integration around `podman run`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LoadResult {
    /// `podman run` → `/health` 200, container start included.
    pub ms: u64,
    /// Device VRAM used once healthy.
    pub vram_used_bytes: Option<u64>,
    /// `vram_used_bytes` minus the baseline (§3.2 step 4).
    pub vram_bytes: Option<u64>,
}

/// What the live server says about itself (§3.4): the numbers every point is
/// derived from, and the build facts `/props` carries on engines that report
/// them (§2.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ServerFacts {
    /// *N_slots*: `/props` `total_slots`, else the length of `/slots`.
    pub n_slots: u32,
    /// *S*: the per-slot context.
    pub per_slot_ctx: u64,
    /// Where *S* was read: `slots`, `props.default_generation_settings.n_ctx`
    /// or `props.n_ctx / total_slots`.
    pub per_slot_ctx_source: String,
    /// The whole context, where the engine reports it (ik's top-level
    /// `/props` `n_ctx`).
    pub total_ctx: Option<u64>,
    /// Whether every slot draws its KV cells from one shared pool (§13
    /// decision 57): then *S* is the whole pool, and requests running
    /// together must fit in it side by side — a full pool aborts every
    /// running request. From the server where it says (ik's top-level
    /// `n_ctx` against the per-slot one), else from the row's effective
    /// settings (`kv_unified`, or `parallel` left on auto); `None` when
    /// neither says, read as split.
    pub kv_unified: Option<bool>,
    /// Where `kv_unified` was read: `props.n_ctx` or `settings`.
    pub kv_unified_source: Option<String>,
    /// `/props` `build_info` (official llama.cpp only).
    pub build_info: Option<String>,
    /// `/props` `model_ftype` (official llama.cpp only).
    pub model_ftype: Option<String>,
    /// Whether any slot reports speculative decoding (official `/slots`).
    pub speculative: Option<bool>,
    /// `/props` `chat_template_caps`, the boolean entries. Empty when the
    /// engine sends none (ik).
    pub chat_template_caps: BTreeMap<String, bool>,
    /// `/props` `modalities.vision`.
    pub vision: Option<bool>,
}

/// A phase that ended with an error. The points measured before it are kept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PhaseError {
    pub phase: Phase,
    pub error: String,
}

/// A request the engine sent a second time because its connection broke
/// before any response arrived (benchmark design §13 decision 62). What the
/// step recorded is the second attempt's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RetriedRequest {
    /// The step it belonged to, as the progress line named it
    /// ("prefill: 32768 tokens, repetition 1/3").
    pub stage: String,
    /// The request, e.g. `POST /completion`.
    pub request: String,
    /// Why the first attempt failed, with its whole cause chain.
    pub error: String,
}

/// The run's VRAM and energy picture (§4.3, §7 `results`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EnergySummary {
    /// How the run's joules were measured; `None` when the GPU reports no
    /// power at all (then `unavailable` says why).
    pub source: Option<EnergySource>,
    pub unavailable: Option<String>,
    /// Device VRAM used before the bench container started (§3.2 step 4).
    pub baseline_vram_bytes: Option<u64>,
    /// Mean power over the baseline window, so a view can show net energy.
    pub idle_power_w: Option<f64>,
    pub peak_vram_bytes: Option<u64>,
    pub peak_power_w: Option<f64>,
    /// First sample to last.
    pub total_joules: Option<f64>,
}

/// Everything a run measured apart from the probes and the timeline (§7
/// `results`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RunResults {
    /// True once every selected phase finished without an error. A crash, a
    /// cancel or a phase error leaves it false: the numbers present are real,
    /// the set is partial.
    pub complete: bool,
    pub server: Option<ServerFacts>,
    pub points: Option<PointPlan>,
    /// The corpus, tokenized by the bench server (§4.1).
    pub corpus_tokens: Option<u64>,
    /// The token ids every measured prompt starts with: the vocabulary's
    /// BOS where it adds one — llama-server adds none to a token-id prompt —
    /// counted inside each prompt's length (§13 decision 58). Empty when the
    /// vocabulary adds nothing.
    pub prompt_prefix: Vec<u32>,
    pub load: Option<LoadResult>,
    pub prefill: Vec<PrefillPoint>,
    pub decode: Vec<DecodePoint>,
    pub concurrent: Vec<ConcurrentPoint>,
    pub mixed: Option<MixedResult>,
    /// Phases that ran to the end, in order.
    pub phases_done: Vec<Phase>,
    pub phase_errors: Vec<PhaseError>,
    /// Requests sent a second time after a broken connection (decision 62),
    /// in the order they happened.
    pub retried: Vec<RetriedRequest>,
    pub energy: EnergySummary,
}

/// The behaviour probes (§5).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ProbeKind {
    #[default]
    Chat,
    ThinkingOff,
    ThinkingOn,
    ToolCall,
    JsonSchema,
    Deterministic,
    Vision,
    ReasoningHistory,
    /// Always last (§4.4): it is the one probe with a full-context prefill.
    Needle,
}

impl ProbeKind {
    pub const ALL: [ProbeKind; 9] = [
        ProbeKind::Chat,
        ProbeKind::ThinkingOff,
        ProbeKind::ThinkingOn,
        ProbeKind::ToolCall,
        ProbeKind::JsonSchema,
        ProbeKind::Deterministic,
        ProbeKind::Vision,
        ProbeKind::ReasoningHistory,
        ProbeKind::Needle,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProbeKind::Chat => "chat",
            ProbeKind::ThinkingOff => "thinking_off",
            ProbeKind::ThinkingOn => "thinking_on",
            ProbeKind::ToolCall => "tool_call",
            ProbeKind::JsonSchema => "json_schema",
            ProbeKind::Deterministic => "deterministic",
            ProbeKind::Vision => "vision",
            ProbeKind::ReasoningHistory => "reasoning_history",
            ProbeKind::Needle => "needle",
        }
    }
}

/// How a probe ended (§5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ProbeOutcome {
    Pass,
    Fail,
    /// A finding, not a verdict (`reasoning_history`).
    Info,
    /// Not applicable to this model; `detail` says why.
    #[default]
    Skipped,
    /// The server answered with an error status, or not at all.
    Error,
}

/// One response a probe judged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProbeEvidence {
    /// The HTTP status; `None` when the request never got one.
    pub status: Option<u16>,
    /// `choices[0].message.content`.
    pub content: Option<String>,
    /// `choices[0].message.reasoning_content`.
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
    /// `choices[0].message.tool_calls`, verbatim.
    pub tool_calls: Option<serde_json::Value>,
    /// The raw body when it is not a chat completion: an error status's body,
    /// `/apply-template`'s rendered prompt.
    pub body: Option<String>,
    pub ms: Option<u64>,
}

/// One probe's result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProbeResult {
    pub probe: ProbeKind,
    pub outcome: ProbeOutcome,
    /// One line: what passed or failed, the skip reason, or the error.
    pub detail: String,
    /// The responses judged — one, or four for `deterministic` (the cached
    /// pair, then the no-cache pair).
    pub evidence: Vec<ProbeEvidence>,
}

/// §7 `probes`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProbeReport {
    pub probes: Vec<ProbeResult>,
}

/// One stored sample (every [`TIMELINE_INTERVAL_MS`]); sums over the devices,
/// temperature is the hottest one's.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TimelineSample {
    /// Since the sampler started.
    pub t_ms: u64,
    pub vram_used_bytes: Option<u64>,
    pub power_w: Option<f64>,
    pub temp_c: Option<u32>,
    /// The OR of [`CLOCK_EVENT_REASONS`] bits seen since the previous stored
    /// sample.
    pub clock_events: Option<u64>,
}

/// A phase's band on the timeline.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PhaseSpan {
    pub phase: Phase,
    pub start_ms: u64,
    /// `None` while the phase is still running.
    pub end_ms: Option<u64>,
}

/// §7 `timeline`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Timeline {
    pub interval_ms: u64,
    pub samples: Vec<TimelineSample>,
    pub phases: Vec<PhaseSpan>,
}

/// §6 `gpu`, as far as the sampler sees it. Sums and maxima over the visible
/// devices.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GpuIdentity {
    /// The first device's name (comparison key, §6).
    pub name: Option<String>,
    pub driver: Option<String>,
    pub devices: u32,
    pub vram_total_bytes: Option<u64>,
    /// The enforced power limit.
    pub power_limit_w: Option<f64>,
    pub temp_start_c: Option<u32>,
    pub temp_end_c: Option<u32>,
    pub temp_max_c: Option<u32>,
    /// The OR of every sample's clock-event reasons.
    pub clock_events: u64,
    /// Whether any of those is a [`THROTTLE_MASK`] reason. A run that
    /// throttled reports the throttled numbers.
    pub throttled: bool,
    /// The GPU probe's own label (`NVML (driver …)`, or why there is none).
    pub telemetry: String,
}

/// §6 `model`, filled by the integration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelIdentity {
    pub model_id: String,
    pub gguf_path: String,
    pub gguf_size: u64,
    pub gguf_mtime: Option<String>,
    /// From the GGUF header's file type.
    pub quant: Option<String>,
    /// A ladder row's rung, 0 = base.
    pub rung: u32,
}

/// §6 `build`, filled by the integration from the image and its labels
/// (§2.2), plus `/props` `build_info` where the engine reports it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildIdentity {
    pub image_ref: String,
    pub image_id: Option<String>,
    /// `dev.lmgw.slug`.
    pub engine_slug: Option<String>,
    /// `dev.lmgw.repo`.
    pub repo: Option<String>,
    /// `dev.lmgw.base`, else `org.opencontainers.image.revision`.
    pub commit: Option<String>,
    /// `dev.lmgw.ref`.
    pub git_ref: Option<String>,
    /// `org.opencontainers.image.version`.
    pub version: Option<String>,
    pub build_info: Option<String>,
    /// Every label, verbatim.
    pub labels: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_list_is_ordered_deduplicated_and_always_loads() {
        assert_eq!(
            Phase::parse_list("mixed, probes,probes").unwrap(),
            vec![Phase::Load, Phase::Probes, Phase::Mixed]
        );
        assert_eq!(Phase::parse_list("").unwrap(), Phase::ALL.to_vec());
        assert_eq!(Phase::parse_list("load").unwrap(), vec![Phase::Load]);
        let err = Phase::parse_list("prefil").unwrap_err();
        assert!(err.contains("prefil") && err.contains("prefill"), "{err}");
    }

    #[test]
    fn suite_params_v1_carries_the_constants() {
        let p = SuiteParams::v1(0, vec![Phase::Decode]);
        assert_eq!(p.repetitions, 1);
        assert_eq!(p.phases, vec![Phase::Load, Phase::Decode]);
        assert_eq!(p.generate_tokens, 256);
        assert_eq!(p.sampling.seed, 1234);
        assert!(p.has(Phase::Load) && !p.has(Phase::Mixed));
    }

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProbeKind::ReasoningHistory).unwrap(),
            "\"reasoning_history\""
        );
        for k in ProbeKind::ALL {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
        for p in Phase::ALL {
            assert_eq!(serde_json::to_value(p).unwrap(), p.as_str());
        }
        assert_eq!(
            serde_json::to_string(&EnergySource::Integrated).unwrap(),
            "\"integrated\""
        );
    }

    #[test]
    fn a_partial_row_still_reads() {
        let r: RunResults =
            serde_json::from_str(r#"{"complete":false,"prefill":[{"prompt_tokens":512}]}"#)
                .unwrap();
        assert_eq!(r.prefill[0].prompt_tokens, 512);
        assert!(r.prefill[0].energy.is_none());
        let p: SuiteParams = serde_json::from_str(r#"{"repetitions":5}"#).unwrap();
        assert_eq!(p.repetitions, 5);
        assert_eq!(p.suite_version, SUITE_VERSION);
    }

    #[test]
    fn throttle_mask_matches_the_table() {
        let mask = CLOCK_EVENT_REASONS
            .iter()
            .filter(|(_, _, t)| *t)
            .fold(0, |m, (b, _, _)| m | b);
        assert_eq!(mask, THROTTLE_MASK);
    }
}
