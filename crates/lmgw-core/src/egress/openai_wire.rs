//! The OpenAI wire pieces an OpenAI-shaped egress is composed of (llama-egress
//! design §2): the chat body builder and its request, the `messages` array,
//! the response and stream readers (a Responses `Response` object's too), the
//! auxiliary endpoints' builders and error mapping, and System One's request.
//! An egress passes the builder the two parts its dialect owns,
//! a [`ReasoningStep`] and a [`ToolResultRenderer`], and implements each other
//! trait method with one call into this module. [`super::openai`] and
//! [`super::llama_cpp`] are composed this way: a second egress speaking the
//! same wire composes the same pieces rather than forking them.

mod aux;
mod body;
mod decode;
mod messages;
mod response_object;
pub mod responses_items;
pub mod structured;
mod systemone;

pub use aux::{
    build_embeddings, build_rerank, context_refusal, map_error, parse_embeddings,
    parse_exceed_context, parse_rerank, parse_tokenize_count, upstream_error, ExceedContext,
};
pub(crate) use body::has_reasoning_object;
pub use body::{build_chat_body, chat_request, BodyDialect, ReasoningStep};
pub use decode::{parse_completion, parse_completion_with, OpenaiDecoder, ReadTier};
pub(crate) use decode::{parse_timings, parse_usage};
pub use messages::{audio_format, messages_json, FlattenToolResults, ToolResultRenderer};
pub(crate) use response_object::{is_billable, read_response, ResponseReport, TERMINAL_EVENTS};
pub use systemone::{build_systemone, SystemOneBody};
