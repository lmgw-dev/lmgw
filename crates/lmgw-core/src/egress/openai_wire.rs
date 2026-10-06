//! The OpenAI wire pieces an OpenAI-shaped egress is composed of (llama-egress
//! design §2): the chat body builder and its request, the `messages` array,
//! the response and stream readers, and the auxiliary endpoints' builders and
//! error mapping. An egress passes the builder the two parts its dialect owns,
//! a [`ReasoningStep`] and a [`ToolResultRenderer`], and implements each other
//! trait method with one call into this module. [`super::openai`] and
//! [`super::llama_cpp`] are composed this way: a second egress speaking the
//! same wire composes the same pieces rather than forking them.

mod aux;
mod body;
mod decode;
mod messages;

pub use aux::{
    build_embeddings, build_rerank, context_refusal, map_error, parse_embeddings,
    parse_exceed_context, parse_rerank, parse_tokenize_count, upstream_error, ExceedContext,
};
pub(crate) use body::has_reasoning_object;
pub use body::{build_chat_body, chat_request, ReasoningStep};
pub(crate) use decode::parse_timings;
pub use decode::{parse_completion, OpenaiDecoder};
pub use messages::{messages_json, FlattenToolResults, ToolResultRenderer};
