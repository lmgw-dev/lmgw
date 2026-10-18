//! The Decision lab (labs-store design §2): the page that asks decision
//! models through `/v1/systemone`. Like [`crate::image_lab`], this module
//! carries a function as well as shapes: [`form::DecisionForm::body`] is the
//! request contract, and the page's preview and the server's dispatch are
//! both its text.

/// What the page posts: an Ask, whose plan the store keeps, and a count.
pub mod ask;
/// Set runs: a question set over a CSV or JSONL file, as a job (§3.3).
pub mod batch;
/// The three built-in forms of the Examples menu.
pub mod examples;
/// The form, its check, its title and the one request builder.
pub mod form;
/// A labelled set run from a batch agent's apply run (§4).
pub mod from_agent;
/// A set run's metrics (§3.4).
pub mod metrics;
/// The model list and its facts and limits.
pub mod models;
/// Question sets and their thresholds (§3.2).
pub mod sets;

pub use ask::{
    DecisionAsk, DecisionCount, DecisionCounted, DecisionEstimate, QuestionTokens,
    PERTURB_WITH_OVERRIDE,
};
pub use batch::{
    job_key, read_label, score_column, BatchColumn, BatchFormat, BatchItemPage, BatchItemsQuery,
    BatchJobDetail, BatchList, BatchMapping, BatchPreview, BatchPreviewed, BatchProblem,
    BatchProblemKind, BatchResume, BatchStart, BatchStatus, BatchView, ScoreColumn, ScoreLabels,
    ThresholdWrite, PREVIEW_ROWS,
};
pub use examples::{examples, Example};
pub use form::{
    check_questions, DecisionForm, ImageInput, OptionDraft, QuestionDraft, QuestionKind, StateMode,
    Variant,
};
pub use from_agent::{
    agent_source_name, BatchFromAgent, BatchFromAgentSaved, BatchFromAgentSkip,
    BatchFromAgentSkipped, AGENT_KEY_COLUMN, AGENT_STATE_COLUMN,
};
pub use metrics::{
    BatchMetrics, CurvePoint, MetricsQuery, QuestionMetrics, ReliabilityBin, DEFAULT_BINS,
};
pub use models::{
    CountSource, DecisionBackend, DecisionFacts, DecisionFallbackPrice, DecisionLabModel,
    DecisionLabModels, DecisionLimit, DecisionMeasure, DecisionPrice, DecisionShape,
};
pub use sets::{Measured, QuestionSet, Threshold};
