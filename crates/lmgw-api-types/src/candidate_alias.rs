//! `CandidateAliasView` (candidate-aliases design §4.1, §4.6): the stored
//! shape of a `candidate_aliases` row plus its live derivation
//! (`candidates::derive` in lmgw-core), joined the way `GET /api/models/full`
//! and the alias editor need it. Its own file (not `lib.rs`) because the
//! derived half alone is a dozen-plus fields — kept out of the already large
//! DTO file per the modularisation note, mirroring how [`crate::builds`] and
//! [`crate::image_lab`] get their own modules.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Mirror of `config::CandidateAlias` (stored fields) plus what
/// `candidates::derive::derive` computes from the live snapshot (never
/// stored, recomputed on every read — see that function's own doc comment
/// for the cost).
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
    /// The owner's explicit "turn this common facet off" list (wire facet
    /// names).
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
    pub routable: Vec<String>,
    /// What is wrong with a candidate or the fallback, in prose.
    pub problems: Vec<String>,
    /// A candidate with `--cache-ram 0` (§4.5): not an error, but worth
    /// flagging.
    pub advisories: Vec<String>,
    /// The alias fallback resolves, is not local, and supports every enabled
    /// facet.
    pub fallback_usable: bool,
    /// Minimum across `routable` (a ladder candidate at its top rung);
    /// absent if any of them is unknown or nothing is routable.
    pub context_length: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// How many `gpu_deferred` refusals this alias logged in the last 24
    /// hours (§6) — `lmgw_core::candidates::deferrals`. `Some(0)` is a real
    /// zero; `None` only ever means a frame cached from before this field
    /// existed (`#[serde(default)]`), never "unknown".
    #[serde(default)]
    pub deferrals_24h: Option<u64>,
}
