//! Proxy engine (§4): ingress IR → router → egress adapter → upstream,
//! translating streaming and non-streaming responses back into the client's
//! protocol, with telemetry on every request.
//!
//! Routing itself is [`Snapshot::resolve`]; param merge precedence is
//! client > alias overrides (see [`Params::with_defaults`]).

mod recording;
pub use recording::*;
mod in_process;
pub(crate) use in_process::*;
mod chat;
pub use chat::*;
mod chat_stream;
pub(crate) use chat_stream::*;
mod legacy;
pub use legacy::*;
mod count;
pub use count::*;
mod count_messages;
pub use count_messages::*;
mod embeddings;
pub use embeddings::*;
mod audio;
pub use audio::*;
mod multipart;
use multipart::*;
mod transcribe;
pub use transcribe::transcribe;
mod image;
pub use image::*;
mod route_guards;
pub(crate) use route_guards::*;
// Public for `server::build_router`, which merges its `routes()` at the root.
pub mod tokenize;

#[cfg(test)]
mod tests;
