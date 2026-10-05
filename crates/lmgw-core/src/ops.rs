//! Protocol-agnostic gateway administration (§20).
//!
//! One function per admin action, each `(&SharedState, typed patch) ->
//! Result<Value, String>`. This is exactly what the HTML handlers in [`web`]
//! already do — validate, persist through [`store`], `reload_snapshot()`, poke
//! the affected manager — with the redirect-and-flash removed, so a *second*
//! front end can drive the gateway without re-implementing any of it.
//!
//! Today's consumer is the built-in self-admin MCP tool plane
//! ([`crate::mcp::selfadmin`]). A CLI would be the same functions behind clap.
//!
//! Two conventions hold throughout:
//!
//! - **Patches are sparse.** Every field is `Option`; `None` means "leave as
//!   is". An `update` loads the current row, overlays the provided fields, and
//!   writes it back, so a caller never has to restate the whole record.
//! - **Secrets go out redacted, never round-tripped.** API keys, HF/update
//!   tokens, and MCP `env`/`headers` *values* are replaced by `<set>` on the
//!   way out. Because updates merge against the stored row rather than against
//!   what was read, a redacted read followed by a partial write cannot
//!   overwrite a secret with the placeholder.
//!
//! [`web`]: crate::web
//! [`store`]: crate::store

pub mod backends;
/// The benchmark ops (benchmark design §8.1), typed like [`backends`].
pub mod bench;
/// `candidate_alias_set` (candidate-aliases design §4.1, §4.6) — a child
/// module for the same file-size reason as [`backends`]; the small shared
/// helpers it reaches through `super::` (`candidate_alias_name_taken`,
/// `refuse_if_candidate_alias_name`, the extended `validate_fallback_alias`)
/// live in `common` and `settings_patch` and come through the re-exports below.
mod candidate_alias;
pub use candidate_alias::{candidate_alias_set, CandidateAliasPatch};
mod common;
pub use common::*;
mod reads;
pub use reads::*;
mod routing;
pub use routing::*;
mod ladder_checks;
pub use ladder_checks::*;
mod local_model;
pub use local_model::*;
mod local_model_checks;
pub use local_model_checks::*;
mod aux_model;
pub use aux_model::*;
mod image_model;
pub use image_model::*;
mod audio_model;
pub use audio_model::*;
mod audio_tools;
pub use audio_tools::*;
mod voice_library;
pub use voice_library::*;
mod recipes;
pub use recipes::*;
mod huggingface;
pub use huggingface::*;
mod mcp_server;
pub use mcp_server::*;
mod hold;
pub use hold::*;
mod container_actions;
pub use container_actions::*;
mod settings_patch;
pub use settings_patch::*;
mod chat_voice_settings;
pub use chat_voice_settings::*;
mod realtime_settings;
pub use realtime_settings::*;
mod realtime_budget;
pub use realtime_budget::*;
mod keys;
pub use keys::*;
mod usage_prices;
pub use usage_prices::*;
mod docs_corpora;
pub use docs_corpora::*;
mod agent_catalog;
pub use agent_catalog::*;
#[cfg(test)]
mod tests;
