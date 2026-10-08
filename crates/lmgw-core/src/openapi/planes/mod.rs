//! The route registry, one file per plane (api-docs design §3.2). Each
//! non-hub file here holds a `pub(crate) fn routes() -> Vec<DocRoute>`, read
//! by [`super::registry::all_routes`].
//!
//! Every module below is `pub(crate)`, not private: `registry::all_routes`
//! and `endpoints::lmgw_endpoints` (both siblings of `planes` under
//! `openapi`, not descendants of it) call each plane's `routes()` directly.

pub(crate) mod agents;
pub(crate) mod chat;
pub(crate) mod dashboard;
pub(crate) mod docs;
pub(crate) mod inference;
pub(crate) mod session;
pub(crate) mod usage;
