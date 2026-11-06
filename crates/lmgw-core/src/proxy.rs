//! Proxy engine (§4): ingress IR → router → egress adapter → upstream,
//! translating streaming and non-streaming responses back into the client's
//! protocol, with telemetry on every request.
//!
//! Routing itself is [`Snapshot::resolve`]; param merge precedence is
//! client > alias overrides (see [`Params::with_defaults`]).

pub(crate) mod answer_items;
mod recording;
pub(crate) mod seals;
pub use recording::*;
mod in_process;
pub(crate) use in_process::*;
mod chat;
pub use chat::*;
mod chat_stream;
pub(crate) use chat_stream::*;
mod legacy;
pub(crate) mod reasoning_fit;
pub use legacy::*;
mod count;
pub use count::*;
mod count_admit;
use count_admit::*;
mod count_messages;
pub use count_messages::*;
mod count_responses;
pub use count_responses::*;
mod embeddings;
pub use embeddings::*;
mod audio;
pub use audio::*;
mod multipart;
use multipart::*;
mod transcribe;
pub use transcribe::{
    answered_line, transcribe, transcribe_as, transcribe_voice_clip, ClipTranscript,
};
pub(crate) use transcribe::{
    transcribe_dictation, transcribe_for, transcribe_turn, warm as transcribe_warm, ASR_REQUIRED,
};
mod image;
// The image tools' in-process call (chat-media-tools design §4.1).
pub(crate) mod image_call;
pub(crate) mod synthesize;
pub use image::*;
mod route_guards;
pub(crate) use route_guards::*;
mod relay;
pub(crate) mod silence;
mod unanswered;
pub(crate) use unanswered::Unanswered;
pub use unanswered::{CLIENT_CLOSED_STATUS, CLIENT_DISCONNECTED};
mod stop;
#[cfg(test)]
pub(crate) use stop::stopped_usage;
pub(crate) use stop::{
    canceled, floor_count, is_canceled, produced_chars, row_status, stopped, stopped_cost,
    unanswered_cost,
};
pub use stop::{stop_pair, StopHandle, StopSignal};
// Public for `server::build_router`, which registers its handler with the
// `/v1` JSON routes.
pub mod systemone;
// Public for `server::build_router`, which merges its `routes()` at the root.
pub mod tokenize;

#[cfg(test)]
mod tests;
