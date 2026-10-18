//! `/labs/api/*`, the lab store's routes (labs-store design §1.6): history,
//! delete, clear, usage, orphans, export, artifacts and presets, the same
//! operations for all three labs. `{lab}` is a `LabKind` (`decision`, `image`,
//! `audio`); anything else is a 404. Dispatch stays per lab
//! (`/decision-lab/api/*`), since each lab turns its own form into its own
//! request.
//!
//! Errors are the dashboard's flat `ApiError`, as on `/chat/api`: the
//! extractors are the Chat API's, which answer a malformed path or query in
//! that shape too.

/// The artifact route: the bytes with their type, sandboxed.
pub(super) mod artifacts;
/// An unknown lab's 404 and a store failure's 500.
mod errors;
/// The export route.
pub(super) mod export;
/// History, delete, clear, usage and orphans.
pub(super) mod groups;
/// Presets: a lab's named form parts (question sets).
pub(super) mod presets;

pub(super) use errors::{lab_of, store_failed};
