//! The answers only the self-admin tools (`lmgw__*`, served on `/mcp/admin`)
//! give. They are not in the OpenAPI document, which describes the HTTP API;
//! the tool's text is the answer's JSON, and a test reads that text back into
//! the type and writes it out equal.
//!
//! One child module per tool group.

mod agents;
pub use agents::*;
mod audio;
pub use audio::*;
mod builds;
pub use builds::*;
mod chat_profiles;
pub use chat_profiles::*;
mod docs;
pub use docs::*;
mod logs;
pub use logs::*;
mod models;
pub use models::*;
mod settings;
pub use settings::*;
mod usage;
pub use usage::*;
