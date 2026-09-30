//! The op table: one [`OpDoc`] per name in [`crate::web::op_names::all`]
//! (api-docs design §4.7). Split by `ops-*` tag group (file-size discipline):
//! each `table/*.rs` file owns one group's `pub(super) const OPS`, and
//! [`table`] concatenates them in [`crate::web::op_names::all`]'s own order
//! (`MAIN_OPS`, `AGENT_OPS`, `KEY_OPS`, `SETTINGS_OPS`) — `merge.rs` (the
//! tool ↔ struct doc merge), `args.rs` (hand-written `Hand` schemas) and
//! `divergence.rs` (`DIVERGENCES`) are its siblings.

use super::super::registry::{Resp, SchemaFn};

mod agents;
mod bench;
mod builds;
mod downloads;
mod keys;
mod models;
mod responses;
mod routing;
mod runtime;
mod settings;
mod tools;
mod usage;

/// Where an op's argument schema comes from (§4.7).
#[derive(Debug, Clone, Copy)]
pub(crate) enum OpArgs {
    /// The struct the dispatcher parses into (`ops::patch_from_args`) —
    /// finding §0.1: this, not the tool's flat projection, is the op's real
    /// contract.
    Struct(SchemaFn),
    /// No struct of its own; the tool's `inputSchema` is the documented
    /// shape (table A's "same" rows) — [`super::merge::tool_schema`].
    Tool,
    /// Neither: a worker read the arm or function body and wrote the JSON
    /// Schema by hand (table D, [`super::args`]).
    Hand(fn() -> serde_json::Value),
    NoArgs,
    /// A deprecated alias of another op (`embed_model_set` → `aux_model_set`).
    AliasOf(&'static str),
}

/// One documented op (§4.7).
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpDoc {
    pub name: &'static str,
    /// An `ops-*` `TAGS` id.
    pub tag: &'static str,
    pub summary: &'static str,
    /// `None` → the shared tool's description (the merge rule, §4.7,
    /// [`super::merge::description`]).
    pub description: Option<&'static str>,
    pub tool: Option<&'static str>,
    pub args: OpArgs,
    pub response: Resp,
    pub writes: bool,
    pub reveals_secret: bool,
    pub confirm_note: Option<&'static str>,
    pub deprecated: bool,
    /// A JSON example as text — an op's example is typically hand-written
    /// only where the generated one (§4.10) would not be representative.
    pub example: Option<&'static str>,
}

/// Every op, in [`crate::web::op_names::all`]'s order — 81 in total (§4.7).
/// `openapi_ops.rs`'s `every_listed_op_is_documented_and_vice_versa` is what
/// proves the two agree.
pub(crate) fn table() -> Vec<OpDoc> {
    [
        routing::OPS,
        models::OPS,
        runtime::OPS,
        downloads::OPS,
        settings::OPS,
        tools::OPS,
        usage::OPS,
        responses::OPS,
        builds::OPS,
        bench::OPS,
        agents::OPS,
        keys::OPS,
    ]
    .concat()
}
