//! Candidate-alias derivation (candidate-aliases design §4.6): what a saved
//! `CandidateAlias` row means *right now* — which facets its candidates have
//! in common, which of them the gate may actually route to, and what is
//! wrong with the rest.
//!
//! - [`facets`] is pure — the wire vocabulary, a `Copy` bitset over it, and
//!   the positive-support rule, all unit-testable on hand-built rows.
//! - [`derive`] is the I/O half: [`derive::derive`] reads every candidate's
//!   and the fallback's live capabilities (through
//!   [`crate::capabilities::exposed`], which owns the GGUF cache and the
//!   upstream catalog cache) and folds them into what the editor,
//!   `/v1/models`, `lmgw__models` and the gate worker each need.
//!   [`derive::cached_pick`] is the per-request entry point that costs no
//!   read once a snapshot's picks are warm — only a `stat` of each file the
//!   pick was derived from ([`stamps`]), so a GGUF replaced on disk is
//!   noticed without a config write.
//! - [`validate`] is the pure save-time decision rule (§4.6's three
//!   capability refusals, folded into one computation) that `ops`'
//!   `candidate_alias` module calls.
//! - [`deferrals`] answers a different question — not what an alias means,
//!   but what happened to it: how many `gpu_deferred` refusals it logged in
//!   the last 24 hours (§6), read once for every alias at a time.
//!
//! **Positive support only (§4.6, fact 13).** A facet counts as supported
//! exactly when the model's published capabilities say so positively —
//! absent is unknown, and unknown counts as unsupported, the same rule the
//! capability schema states for itself. A row whose capabilities could not
//! be derived at all (unreadable GGUF) therefore supports nothing.
//!
//! **The enabled set is never recomputed outside a save.** `CandidateAlias::
//! capabilities_enabled` is what `ops::candidate_alias` last computed and
//! stored (via [`validate::resolve_enabled`]); [`derive::derive`] and
//! [`derive::cached_pick`] only ever read it — see that field's doc comment
//! for why (§12 entry 49).

pub mod deferrals;
pub mod derive;
pub mod facets;
mod stamps;
pub mod validate;

pub use facets::{supports, Facet, FacetSet};
