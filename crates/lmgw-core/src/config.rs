//! Domain/config types and the immutable in-memory snapshot (§9).
//!
//! Config is cached in an `Arc<Snapshot>` swapped atomically on edit, so the
//! request hot path never reads the database.

mod routes;
pub use routes::*;
mod llama_route;
pub use llama_route::*;
mod models;
pub use models::*;
mod llama_params;
pub use llama_params::*;
mod mcp;
pub use mcp::*;
mod keys;
pub use keys::*;
mod settings;
pub use settings::*;
mod settings_classes;
pub use settings_classes::*;
mod semantic_vad;
pub use semantic_vad::*;
mod chat_prompt;
pub use chat_prompt::*;
mod prices;
pub use prices::*;
mod snapshot;
pub use snapshot::*;

#[cfg(test)]
mod tests;
