//! The ubatch a projector needs, as a plan proposes it: the chat plan's and
//! the aux plan's (a multimodal embedder, an image-capable decision model)
//! alike. When llama.cpp decodes a projector's images with non-causal
//! attention, a whole image has to fit one physical batch, and the floor is
//! [`super::projector_ubatch_floor`]'s.

use serde_json::{json, Value};

use super::{projector_ubatch_floor, ImageAttention};
use crate::runtime::aux::{LLAMA_DEFAULT_BATCH, LLAMA_DEFAULT_UBATCH};

/// Put the `ubatch_size` (and, past llama.cpp's default batch, the
/// `batch_size`) the projector decoded as `image` needs, when it needs more
/// than the plan gives it so far: `planned` is the plan's ubatch and batch,
/// `None` where it sets none and llama-server's default applies. A causal
/// projector splits an image across physical batches and needs nothing; a
/// larger ubatch would only cost compute buffer.
pub(super) fn plan(
    image: Option<&ImageAttention>,
    planned: (Option<i64>, Option<i64>),
    put: &mut dyn FnMut(&str, Value, String),
) {
    let lead = match image {
        Some(ImageAttention::NonCausal { ty, .. }) => format!(
            "llama.cpp decodes this {ty} projector's images with non-causal attention, so a \
             whole image has to fit in one physical batch"
        ),
        Some(ImageAttention::Unknown(why)) => format!(
            "lmgw could not tell whether llama.cpp decodes this projector's images with \
             non-causal attention ({why}), so this is a precaution: if it does, a whole image \
             has to fit in one physical batch"
        ),
        Some(ImageAttention::Causal) | None => return,
    };
    let Some((need, basis)) = image.and_then(|a| projector_ubatch_floor(a, &[])) else {
        return;
    };
    let ubatch = planned.0.unwrap_or(LLAMA_DEFAULT_UBATCH);
    let batch = planned.1.unwrap_or(LLAMA_DEFAULT_BATCH);
    // Only above what the plan already gives it: Gemma 3's fixed 256 tokens
    // fit the default, and a non-causal encoder's planned ubatch (its whole
    // context) usually holds any image.
    if need <= ubatch {
        return;
    }
    let now = match planned.0 {
        Some(n) => format!("at the planned {n}"),
        None => format!("at llama.cpp's default of {LLAMA_DEFAULT_UBATCH}"),
    };
    let batch_note = if need > batch {
        String::new()
    } else {
        match planned.1 {
            Some(b) => format!(". The planned batch_size ({b}) already covers it"),
            None => format!(". The default batch_size ({LLAMA_DEFAULT_BATCH}) already covers it"),
        }
    };
    put(
        "ubatch_size",
        json!(need),
        format!(
            "{lead}, and {now} a larger image aborts the server with GGML_ASSERT \"non-causal \
             attention requires n_ubatch >= n_tokens\". The projector's largest image sets the \
             number: {basis}. No higher, because every ubatch token costs compute buffer in \
             VRAM{batch_note}"
        ),
    );
    if need > batch {
        put(
            "batch_size",
            json!(need),
            format!(
                "at least ubatch_size: mtmd hands an image over in pieces of at most batch_size \
                 tokens, and {} would split this projector's largest image",
                match planned.1 {
                    Some(b) => format!("the planned {b}"),
                    None => format!("llama.cpp's default of {LLAMA_DEFAULT_BATCH}"),
                }
            ),
        );
    }
}
