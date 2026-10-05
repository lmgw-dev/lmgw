//! OpenAI's GA Realtime event protocol as serde types (realtime design §2).
//!
//! GA only (§2.4, §20): the beta names (`response.audio.delta`,
//! `conversation.item.created`, flat `voice`, `modalities`) are not accepted
//! anywhere, and a beta client is refused at the handshake rather than half
//! understood here.

mod client;
mod item;
mod server;
mod session;
mod tools;

pub use client::*;
pub use item::*;
pub use server::*;
pub use session::*;
pub use tools::*;

/// A tagged object whose `type` the GA reference makes optional, with its
/// default filled in — so the derived, tag-dispatched shape rules still
/// apply to everything else in it.
fn with_default_type(mut v: serde_json::Value, default: &str) -> serde_json::Value {
    if let serde_json::Value::Object(m) = &mut v {
        m.entry("type")
            .or_insert_with(|| serde_json::Value::String(default.into()));
    }
    v
}
