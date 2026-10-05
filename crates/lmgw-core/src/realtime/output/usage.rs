//! `response.done.usage` (realtime design §4.3, §11).

use super::super::protocol::{TokenDetails, Usage};

/// [`usage`], or `None` when the upstream reported nothing yet — a
/// cancelled or failed response's `response.done` carries usage only when it
/// is known (§4.3).
pub fn known_usage(u: &crate::ir::Usage) -> Option<Usage> {
    (u.prompt_tokens.is_some() || u.completion_tokens.is_some()).then(|| usage(u))
}

/// `response.done.usage` (§11): the chat model's tokens, all of them text.
pub fn usage(u: &crate::ir::Usage) -> Usage {
    let input = u.prompt_tokens.unwrap_or(0);
    let output = u.completion_tokens.unwrap_or(0);
    Usage {
        total_tokens: input + output,
        input_tokens: input,
        output_tokens: output,
        input_token_details: TokenDetails {
            text_tokens: input,
            audio_tokens: 0,
            cached_tokens: u.cached_input_tokens,
        },
        output_token_details: TokenDetails {
            text_tokens: output,
            audio_tokens: 0,
            cached_tokens: None,
        },
    }
}
