//! Which models `image__edit` offers, and the refusal for one it does not
//! (chat-media-tools design §3.2, §14 item 76).
//!
//! An edit changes a picture as the prompt says. Only two kinds of model do
//! that: a local image row whose `edit` column is set (it takes the picture
//! as `ref_images`), and a cloud alias that publishes `/v1/images/edits`.
//! Any other local row can only take the picture as an img2img start
//! (`init_image` + `strength`), which keeps the composition by
//! construction: asked to "add spaceships" it hands back the same picture,
//! slightly redrawn. So those rows are not in the tool's `model` enum, the
//! tool is not listed when no model can edit, and a call naming one anyway
//! is refused with the models that can. A variation is the card's Vary
//! (`web::chat_media::actions`); the tools have no variation tool yet.

use serde_json::{Map, Value};

use crate::config::ImageModel;

use super::list::Offer;

/// Whether `image__edit` offers the local row `row`.
pub(super) fn edits(row: &ImageModel) -> bool {
    row.edit
}

/// The refusal for an `image__edit` call whose `model` is one of the
/// gateway's image models that cannot edit; `Ok` for any other (an edit
/// model, or a name the `model` check refuses as unknown).
pub(super) fn refuse_non_edit(args: &Map<String, Value>, offer: &Offer) -> Result<(), String> {
    let Some(m) = args.get("model").and_then(Value::as_str) else {
        return Ok(());
    };
    let can_edit = |n: &str| offer.edit.iter().any(|c| c.name == n);
    if can_edit(m) || !offer.generate.iter().any(|c| c.name == m) {
        return Ok(());
    }
    let others = if offer.edit.is_empty() {
        "No model on this gateway can edit pictures: add an edit model under Models → Image \
         (an image row with Edit set), or give an image alias the /v1/images/edits route."
            .to_string()
    } else {
        format!(
            "The models that can edit: {}.",
            offer
                .edit
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Err(format!(
        "model '{m}' cannot edit pictures: it only draws new ones from a prompt, and given a \
         picture it would hand back nearly the same one. {others}"
    ))
}
