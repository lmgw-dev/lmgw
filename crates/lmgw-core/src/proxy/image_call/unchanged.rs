//! A result that is its input (chat-media-tools design §14 item 76): an
//! edit or a variation whose picture comes back byte for byte as the one it
//! started from did nothing — an image server that echoed its input, a
//! route that ignored it. It is the call's error, so no tool result says
//! "made" and no card stores a copy as a new picture.

use crate::error::GatewayError;

use super::{Done, EditKind, ImageCall};

/// `done`, unless one of its pictures is the call's input.
pub(super) fn check(call: &ImageCall<'_>, done: Done) -> Result<Done, GatewayError> {
    let Some(input) = call.action.input() else {
        return Ok(done);
    };
    if !done.images.iter().any(|p| p.bytes == input.bytes) {
        return Ok(done);
    }
    let what = match done.edit {
        Some(EditKind::Variation) => "variation",
        _ => "edit",
    };
    Err(GatewayError::Refused {
        status: 502,
        code: "unchanged_image",
        message: format!(
            "{} returned the input picture unchanged, byte for byte: the {what} did nothing, \
             so no new picture was kept",
            call.alias
        ),
    })
}
