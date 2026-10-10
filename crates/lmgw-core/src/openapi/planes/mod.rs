//! The route registry, one file per plane (api-docs design §3.2). Each
//! non-hub file here holds a `pub(crate) fn routes() -> Vec<DocRoute>`, read
//! by [`super::registry::all_routes`].
//!
//! Every module below is `pub(crate)`, not private: `registry::all_routes`
//! and `endpoints::lmgw_endpoints` (both siblings of `planes` under
//! `openapi`, not descendants of it) call each plane's `routes()` directly.

pub(crate) mod agents;
pub(crate) mod audio_lab;
pub(crate) mod chat;
pub(crate) mod chat_approvals;
/// Merged into [`chat`]'s list, not read by `all_routes` on its own.
pub(crate) mod chat_attachments;
/// Merged into [`chat`]'s list, not read by `all_routes` on its own.
pub(crate) mod chat_profiles;
/// Merged into [`chat`]'s list, not read by `all_routes` on its own.
pub(crate) mod chat_tasks;
/// Merged into [`chat`]'s list, not read by `all_routes` on its own.
pub(crate) mod chat_threads;
/// Merged into [`chat`]'s list, not read by `all_routes` on its own.
pub(crate) mod chat_turns;
pub(crate) mod dashboard;
pub(crate) mod docs;
pub(crate) mod image_lab;
pub(crate) mod inference;
pub(crate) mod knowledge;
pub(crate) mod session;
pub(crate) mod usage;
