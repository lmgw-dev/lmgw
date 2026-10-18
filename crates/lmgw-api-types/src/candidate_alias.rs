//! `CandidateAliasView` (candidate-aliases design §4.1, §4.6): the stored
//! shape of a `candidate_aliases` row plus its live derivation
//! (`candidates::derive` in lmgw-core), joined the way `GET /api/models/full`
//! and the alias editor need it. Its own file (not `lib.rs`) because the
//! derived half alone is a dozen-plus fields — kept out of the already large
//! DTO file per the modularisation note, mirroring how [`crate::builds`] and
//! [`crate::image_lab`] get their own modules.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A candidate alias: its stored fields plus what lmgw computes from the live snapshot
/// (never stored, recomputed on every read).
// Mirror of `config::CandidateAlias`; the derived part is `candidates::derive::derive`,
// whose own doc comment states the cost.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CandidateAliasView {
    pub id: i64,
    pub alias: String,
    /// Local chat model ids, first = primary, rest = alternates in
    /// preference order.
    pub candidates: Vec<String>,
    pub background: bool,
    /// `inherit` | `none` | `alias`.
    pub fallback_mode: String,
    pub fallback: Option<String>,
    /// The explicit "turn this common facet off" list (wire facet names).
    pub capabilities_disabled: Vec<String>,
    pub enabled: bool,
    pub notes: String,

    // --- Derived, from `candidates::derive::CandidateDerived` ---
    /// The stored enabled facet set (`capabilities_enabled` as last saved —
    /// see that field's own doc comment for why this is never recomputed
    /// outside a save).
    pub enabled_facets: Vec<String>,
    /// Facets every candidate currently supports — the ceiling a save may
    /// enable.
    pub common_facets: Vec<String>,
    /// Facet wire name -> candidate ids that do not support it (the editor's
    /// "not supported by: <ids>").
    #[serde(default)]
    pub unsupported_by: HashMap<String, Vec<String>>,
    /// Candidates the gate may actually route to right now, in list order.
    pub routable_candidates: Vec<String>,
    /// What is wrong with a candidate or the fallback, in prose.
    pub problems: Vec<String>,
    /// A candidate with `--cache-ram 0`: not an error, but worth
    /// flagging.
    pub advisories: Vec<String>,
    /// The alias fallback resolves, is not local, and supports every enabled
    /// facet.
    pub fallback_usable: bool,
    /// Minimum across `routable_candidates` (a ladder candidate at its top rung);
    /// absent if any of them is unknown or nothing is routable.
    pub context_length: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// How many `gpu_deferred` refusals this alias logged in the last 24
    /// hours. `Some(0)` is a real
    /// zero; `None` only ever means a frame cached from before this field
    /// existed (`#[serde(default)]`), never "unknown".
    #[serde(default)]
    pub deferrals_24h: Option<u64>,
}

/// One entry of `GatewayStatus.candidate_aliases` (`GET /api/status`,
/// `lmgw__status`): what an alias is, whom it starts and what is wrong with
/// it, from the same derivation the alias editor reads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CandidateAliasStatus {
    /// The alias, as clients name it in `model`.
    pub name: String,
    /// `background` | `owner`: whose traffic the alias serves.
    pub mode: String,
    /// The candidate the alias starts; `null` only for a row that saved with
    /// an empty list.
    pub primary: Option<String>,
    /// The other candidates, in preference order.
    pub alternates: Vec<String>,
    /// The stored enabled facet set (wire facet names).
    pub enabled_facets: Vec<String>,
    /// What is wrong with a candidate or the fallback, in prose.
    pub problems: Vec<String>,
    /// How many `gpu_deferred` refusals the alias logged in the last 24 hours.
    pub deferrals_24h: u64,
}

/// `candidate_alias_set`'s answer to `create`, `update` (also `enable` and
/// `disable`) and `delete`: the saved row's id, and for a write that leaves a
/// row a fresh derivation of what it means right now (the shape the editor
/// and `lmgw__models` read), so a caller never needs a second call to see
/// whether the alias it just saved is usable.
///
/// The derived keys are absent for `delete`, and for a `create` or `update`
/// whose row was deleted by a concurrent call between the write and the read:
/// the write itself succeeded, so that answer is `{ok, id, message}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CandidateAliasSaved {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The row's id.
    pub id: i64,
    /// The stored enabled facet set (wire facet names).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub enabled_facets: Option<Vec<String>>,
    /// Facets every candidate currently supports: the ceiling a save may enable.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub common_facets: Option<Vec<String>>,
    /// Candidates the gate may route to right now, in list order.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub routable_candidates: Option<Vec<String>>,
    /// What is wrong with a candidate or the fallback, in prose.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub problems: Option<Vec<String>>,
    /// Worth flagging, but not an error: a candidate with `--cache-ram 0`, a
    /// fallback that lacks an enabled facet.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub advisories: Option<Vec<String>>,
    /// The alias fallback resolves and is not local.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub fallback_usable: Option<bool>,
    /// Minimum across `routable_candidates`; `null` if any of them is unknown or nothing
    /// is routable. Absent where the derived keys are.
    #[serde(
        deserialize_with = "crate::chat::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub context_length: Option<Option<u64>>,
    /// Same rule as `context_length`.
    #[serde(
        deserialize_with = "crate::chat::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_output_tokens: Option<Option<u64>>,
    /// What was done, for a person to read.
    pub message: String,
}

/// `candidate_alias_set action=preview`: the alias editor's live answer to
/// "what would saving this draft mean right now". It runs the validation and
/// derivation of a save over the draft and writes nothing.
///
/// No single refusal aborts the answer: a draft is the normal state of an
/// editor nobody has saved yet, so the first problem is `error` (and `ok` is
/// `false`) while the derivation goes on with the most useful stand-in it has.
/// Every key is always there.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CandidateAliasPreview {
    /// `false` when `error` is set.
    pub ok: bool,
    /// The facets the draft would have enabled (wire facet names).
    pub enabled_facets: Vec<String>,
    /// Facets every candidate currently supports.
    pub common_facets: Vec<String>,
    /// Facet wire name -> candidate ids that do not support it.
    pub unsupported_by: HashMap<String, Vec<String>>,
    /// Candidates the gate may route to right now, in list order.
    pub routable_candidates: Vec<String>,
    /// What is wrong with a candidate or the fallback, in prose.
    pub problems: Vec<String>,
    /// Worth flagging, but not an error.
    pub advisories: Vec<String>,
    /// The alias fallback resolves and is not local.
    pub fallback_usable: bool,
    /// Minimum across `routable_candidates`; `null` if any of them is unknown or nothing
    /// is routable.
    pub context_length: Option<u64>,
    /// Same rule as `context_length`.
    pub max_output_tokens: Option<u64>,
    /// Every enabled local chat model not yet a candidate, and which of the
    /// enabled facets it would lack if added: the "add candidate" list's
    /// filter.
    pub addable: Vec<AddableCandidate>,
    /// The first refusal a save of this draft would make, if any.
    pub error: Option<String>,
}

/// One local chat model a draft could add as a candidate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AddableCandidate {
    /// The model's id.
    pub id: String,
    /// The enabled facets (wire names) the model does not support; empty when
    /// it can be added as it is.
    pub missing: Vec<String>,
}

/// What `candidate_alias_set` answers, by `action`: [`CandidateAliasPreview`]
/// for `preview`, [`CandidateAliasSaved`] for every other.
///
/// The two shapes share most keys, so this is the document's description of
/// the one op, not a type to read an answer into: a reader knows the action it
/// sent and reads that action's type.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum CandidateAliasAnswer {
    /// `action: preview`.
    Preview(CandidateAliasPreview),
    /// `action: create`, `update`, `enable`, `disable` or `delete`.
    Saved(CandidateAliasSaved),
}
