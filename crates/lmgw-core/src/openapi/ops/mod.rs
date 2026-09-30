//! The op plane's documentation (api-docs design §4.7): one path per
//! `POST /api/op/{name}`, expanded from [`table::table`] rather than listed
//! as a single generic `DocRoute`.
//!
//! `args.rs` holds the hand-written `Hand` argument schemas (table D);
//! `merge.rs` the tool ↔ struct doc merge; `divergence.rs` the pinned
//! `DIVERGENCES` list (table B). `table.rs` itself is split by `ops-*` tag
//! group under `table/`.

pub(crate) mod args;
mod divergence;
mod merge;
mod table;

pub use divergence::{Divergence, DIVERGENCES};
pub(crate) use merge::{description, overlay_tool_props, tool_schema};
pub(crate) use table::{table, OpArgs, OpDoc};
