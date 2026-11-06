//! Chat over OpenAI's Responses wire (openai egress design §7.2, decision 16,
//! WP7): the platform egress's own sends — `/v1/chat/completions`,
//! `/v1/messages`, the synthesized Responses loop and the in-process callers
//! — leave as `POST {base}/responses` rather than `/chat/completions`. The
//! reasoning guide says reasoning models "work better with the Responses
//! API", and Chat Completions "does not support function calling with GPT-6
//! Astra or GPT-6.1 Sol"; on Chat Completions no reasoning text ever comes
//! back.
//!
//! [`wire`] decides per send which wire it takes: the Responses wire unless
//! the send asks for something only Chat Completions has. Native
//! `/v1/responses` passthrough (§6) is not this: it forwards the client's
//! body verbatim.

pub mod body;
pub mod decode;
pub mod wire;

pub use body::responses_body;
pub use decode::{is_response_object, parse_response, PlatformDecoder, ResponsesDecoder};
pub use wire::{chat_only, wire_for, ChatOnly, Wire, CHAT_ONLY};
