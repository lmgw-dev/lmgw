//! The self-admin tool catalog, split one file per domain so no single file
//! holds the whole 54-tool `vec![Builtin { .. }, ..]` literal. [`catalog`]
//! concatenates the groups in the exact order `tools/list` must return them —
//! see each submodule for the tools it owns.

mod agents;
mod bench;
mod builds;
mod docs;
mod models;
mod prices;
mod reads;
mod routing;
mod runtime;
mod sources;

use super::Builtin;

/// The whole built-in tool catalog, in `tools/list` order.
pub(super) fn catalog() -> Vec<Builtin> {
    let mut all = reads::tools();
    all.extend(routing::tools());
    all.extend(models::tools());
    all.extend(sources::tools());
    all.extend(runtime::tools());
    all.extend(prices::tools());
    all.extend(docs::tools());
    all.extend(agents::tools());
    all.extend(builds::tools());
    all.extend(bench::tools());
    all
}
