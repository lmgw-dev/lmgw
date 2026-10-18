//! `GET /decision-lab/api/models` (labs-store design §2.2): every model that
//! answers `/v1/systemone`, with the facts that explain the errors a request
//! can meet and the token limits the gauge compares against.

use serde::{Deserialize, Serialize};

use crate::aux_engine::AuxThreadsSource;

/// The model list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionLabModels {
    pub models: Vec<DecisionLabModel>,
    /// The GPU hold is on.
    pub hold: bool,
    /// The currency of every price row (the gateway's setting).
    pub currency: String,
}

/// One model that answers `/v1/systemone`: a local `decide/*` row, or a
/// cloud alias or catalog entry of task `decision`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionLabModel {
    /// The public name a request asks for.
    pub name: String,
    /// Who serves it, as `GET /v1/models` says (`lmgw` for a local row, the
    /// upstream's name for a cloud model).
    pub owner: String,
    /// A local aux row of kind decision.
    pub local: bool,
    /// The row's id, for a local model.
    pub model_id: Option<String>,
    /// From its capabilities; `null` when unknown.
    pub input_modalities: Option<Vec<String>>,
    /// From its capabilities: the context it is planned against, or the
    /// catalog's.
    pub context_length: Option<u64>,
    /// What `GET /v1/models` notes about it.
    pub notes: Vec<String>,
    /// Its container's state (`starting`, `ready`, `stopping`); `null` when
    /// no container runs, and for a cloud model.
    pub state: Option<String>,
    /// What its container's start found.
    pub warnings: Vec<String>,
    /// The GPU hold (or a benchmark's lease) pauses it now: a GPU row while
    /// the hold is on. A CPU row and a cloud model never are.
    pub held: bool,
    /// The alias that answers for it while it is held, when that alias can.
    pub hold_fallback: Option<String>,
    /// It reads images: its running server's `vision`, else an `image` among
    /// its input modalities.
    pub reads_images: bool,
    /// What its row and its GGUF header say; `null` for a cloud model.
    pub facts: Option<DecisionFacts>,
    /// What must fit in how many tokens (§2.2's table): one or two limits.
    pub limits: Vec<DecisionLimit>,
    /// What prices a request to it, as the gateway prices one.
    pub price: DecisionPrice,
}

/// What prices a request to a decision model: the rows the gateway's own
/// price lookup selects for its primary route (`Snapshot::sheet_for`), the
/// route a request takes when no fallback answers. A fallback that answers
/// is priced by its own rows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionPrice {
    /// Served on this machine: free whatever rows exist (Usage records the
    /// compute it takes, not a price).
    pub local: bool,
    /// Per unit, the row that prices it, each unit resolved on its own: the
    /// alias's manual row, else the alias's catalog row, else the upstream
    /// model's manual row, else its catalog row; a row without a usable rate
    /// is no row. Empty for a local model, and for a cloud model no row
    /// covers, whose cost Usage records as unknown.
    pub rows: Vec<crate::PriceRowView>,
    /// Why its route did not resolve (a candidate alias picks its model per
    /// request); `rows` is empty then.
    pub unresolved: Option<String>,
    /// While a local row is held and its `hold_fallback` can answer, the
    /// price of the alias that answers every request instead (the rows the
    /// gateway then records it at). Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<DecisionFallbackPrice>,
}

/// The price of the alias that answers for a held local row: `alias` and
/// what [`DecisionPrice`] says of a model, for its own primary route.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionFallbackPrice {
    pub alias: String,
    /// Served on this machine: free.
    pub local: bool,
    /// The rows that price it, per unit, as [`DecisionPrice::rows`].
    pub rows: Vec<crate::PriceRowView>,
    /// Why its route did not resolve; `rows` is empty then.
    pub unresolved: Option<String>,
}

/// A decision model's header shape (decision-models design §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DecisionShape {
    /// `attention.causal = false` (Laya, Julia-1): the whole prompt in one
    /// ubatch.
    NonCausal,
    /// Causal with a decision head (Kev, Clef): embedding mode.
    CausalHead,
    /// Causal, answering from label logits (lev, OpenJev): a normal context.
    LabelLogits,
}

impl DecisionShape {
    /// How the head line names it.
    pub fn label(self) -> &'static str {
        match self {
            Self::NonCausal => "non-causal",
            Self::CausalHead => "causal with a head",
            Self::LabelLogits => "label logits",
        }
    }
}

/// Where a ubatch or batch in effect comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum CountSource {
    /// The row's own field.
    Row,
    /// A flag in the row's extra args.
    Args,
    /// The batch: llama.cpp lowers the ubatch to it.
    Batch,
    /// The ubatch: embedding mode (a non-causal type, a causal type with a
    /// head) lowers the batch to it.
    Ubatch,
    /// llama.cpp's default (ubatch 512, batch 2048).
    LlamaCppDefault,
}

impl CountSource {
    /// How a chip names it ("8192 (row)").
    pub fn label(self) -> &'static str {
        match self {
            Self::Row => "row",
            Self::Args => "extra args",
            Self::Batch => "lowered to the batch",
            Self::Ubatch => "lowered to the ubatch",
            Self::LlamaCppDefault => "llama.cpp default",
        }
    }
}

/// Where a local decision row computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DecisionBackend {
    Cpu,
    Gpu,
}

/// A local decision row's facts: its row, its GGUF header and its running
/// server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionFacts {
    /// Why the header could not be read; the header's fields are `null`
    /// then.
    pub header_error: Option<String>,
    /// The header's `decision.type` (`laya`, `kev`, ...).
    pub decision_type: Option<String>,
    /// The header's `decision.max_head_tokens`: past it llama.cpp truncates a
    /// question and its options.
    pub max_head_tokens: Option<u64>,
    pub shape: Option<DecisionShape>,
    /// The row's `ctx_size`; `null` takes llama-server's default.
    pub ctx_size: Option<u64>,
    /// One slot's context as the running server's `/props` says; `null`
    /// until a container has run.
    pub n_ctx_slot: Option<u64>,
    /// The ubatch in effect: the row's, its args', llama.cpp's default, never
    /// above the batch.
    pub ubatch: u64,
    pub ubatch_source: CountSource,
    /// The batch in effect.
    pub batch: u64,
    pub batch_source: CountSource,
    pub backend: DecisionBackend,
    /// The thread count its container runs with; `null` when llama-server
    /// picks.
    pub threads_in_effect: Option<i64>,
    pub threads_source: AuxThreadsSource,
}

/// What a limit bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DecisionMeasure {
    /// The state and one question (with its options and the template): the
    /// gauge's estimate is the state plus the largest question.
    StateAndQuestion,
    /// The span from the first option marker to the end of the prompt: the
    /// estimate is the largest option list.
    OptionSpan,
}

/// One limit a request meets on a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionLimit {
    pub applies_to: DecisionMeasure,
    /// The limit in tokens; `null` when it is not known.
    pub tokens: Option<u64>,
    /// What the number is and where it comes from ("the ubatch in effect,
    /// set on the row").
    pub of: String,
    /// The rule in one sentence, with what llama.cpp answers past it.
    pub rule: String,
}
