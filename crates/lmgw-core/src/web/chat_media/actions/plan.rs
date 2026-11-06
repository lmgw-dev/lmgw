//! What a card action runs, read from the picture it starts from (design
//! §8.3): the model and parameters the picture's `meta.media` says it was
//! made with, and the body's own fields over them. Every refusal is a
//! `422` naming what is missing, before anything runs.

use lmgw_api_types::chat_media::{MediaAction, MediaActionRequest, MediaFacts};
use lmgw_api_types::chat_threads::AttachmentFacts;

use crate::mcp::media::{self, Offer, IMAGE_EDIT, IMAGE_GENERATE};
use crate::proxy::image_call::{Action, InputImage, Params};
use crate::state::SharedState;
use crate::store::ChatAttachmentFull;

/// What a planned action runs.
pub(super) enum Plan {
    /// An image call: a re-roll, a variation or an edit.
    Image {
        /// The tool it is said as (`image__generate`, `image__edit`).
        tool: &'static str,
        /// The model, by the public name the picture was made with.
        alias: String,
        action: Action,
        params: Params,
        /// The `meta.media.parent` of the result: an edit's input, else the
        /// picture the action started from.
        parent: i64,
    },
    /// An upscale.
    Upscale {
        upscaler: Option<String>,
        repeats: Option<u32>,
        made_by: Option<String>,
        rows: Vec<String>,
    },
}

impl Plan {
    /// The model the stream's first frame names.
    pub(super) fn model(&self) -> Option<String> {
        match self {
            Self::Image { alias, .. } => Some(alias.clone()),
            Self::Upscale { .. } => None,
        }
    }
}

/// A refusal: its code and message (`422`).
pub(super) type Refusal = (&'static str, String);

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, Refusal> {
    Err((code, message.into()))
}

/// Plan `req` on the picture `src`, for a caller whose models are `offer`
/// (its key's scope applied). `parent_of` reads another attachment of the
/// same thread (a re-roll of an edit needs the picture the edit started
/// from).
pub(super) async fn plan<F, Fut>(
    state: &SharedState,
    src: &ChatAttachmentFull,
    req: &MediaActionRequest,
    offer: &Offer,
    parent_of: F,
) -> Result<Plan, Refusal>
where
    F: FnOnce(i64) -> Fut,
    Fut: std::future::Future<Output = Option<ChatAttachmentFull>>,
{
    let facts = serde_json::from_value::<AttachmentFacts>(src.meta.clone())
        .ok()
        .and_then(|f| f.media)
        .unwrap_or_default();
    match req.action {
        MediaAction::Upscale => upscale(req, offer, &facts),
        MediaAction::Reroll => reroll(state, src, offer, &facts, parent_of).await,
        MediaAction::Vary | MediaAction::Edit => vary_or_edit(state, src, req, offer, &facts),
    }
}

fn upscale(req: &MediaActionRequest, offer: &Offer, facts: &MediaFacts) -> Result<Plan, Refusal> {
    if offer.upscalers.is_empty() {
        return refuse(
            "no_upscaler",
            "there is no upscaler in the image models dir's upscalers/ — image_upscaler_add \
             downloads one",
        );
    }
    if offer.upscale_rows.is_empty() {
        return refuse(
            "no_image_model",
            "upscaling needs a local image model, and there is none this caller may use",
        );
    }
    if let Some(u) = &req.upscaler {
        media::upscale::check_upscaler(u, offer).or_else(|e| refuse("unknown_upscaler", e))?;
    }
    let repeats = match req.repeats {
        Some(r) => Some(
            media::upscale::check_repeats(i64::from(r)).or_else(|e| refuse("bad_request", e))?,
        ),
        None => None,
    };
    Ok(Plan::Upscale {
        upscaler: req.upscaler.clone(),
        repeats,
        made_by: facts.model.clone(),
        rows: offer.upscale_rows.clone(),
    })
}

/// The model the picture was made with, still offered for `tool`.
fn made_with(facts: &MediaFacts, offer: &Offer, tool: &str) -> Result<String, Refusal> {
    let Some(model) = &facts.model else {
        return refuse(
            "no_model",
            "this picture was not made by one of this gateway's image models, so there is no \
             model to run it again with",
        );
    };
    if offer.choices(tool).iter().any(|c| &c.name == model) {
        return Ok(model.clone());
    }
    refuse(
        "model_unavailable",
        format!(
            "{model}, which made this picture, cannot {} now: it is disabled, removed or not \
             one this caller may use",
            if tool == IMAGE_EDIT {
                "edit pictures"
            } else {
                "draw"
            }
        ),
    )
}

/// The value of parameter `name` the picture was made with, unless the
/// route that made it did not take it.
fn applied<T: Clone>(facts: &MediaFacts, name: &str, v: &Option<T>) -> Option<T> {
    if facts.not_applied.iter().any(|n| n == name) {
        None
    } else {
        v.clone()
    }
}

/// Its settings again, the seed left to be drawn anew.
fn same_settings(facts: &MediaFacts, prompt: String) -> Params {
    Params {
        prompt,
        negative_prompt: applied(facts, "negative_prompt", &facts.negative_prompt),
        size: None,
        seed: None,
        steps: applied(facts, "steps", &facts.steps),
        cfg: applied(facts, "cfg", &facts.cfg),
        strength: applied(facts, "strength", &facts.strength),
    }
}

async fn reroll<F, Fut>(
    state: &SharedState,
    src: &ChatAttachmentFull,
    offer: &Offer,
    facts: &MediaFacts,
    parent_of: F,
) -> Result<Plan, Refusal>
where
    F: FnOnce(i64) -> Fut,
    Fut: std::future::Future<Output = Option<ChatAttachmentFull>>,
{
    let tool = match facts.tool.as_deref() {
        Some(IMAGE_GENERATE) => IMAGE_GENERATE,
        Some(IMAGE_EDIT) => IMAGE_EDIT,
        Some(other) => {
            return refuse(
                "not_rerollable",
                format!(
                    "this picture was made by {other}, which has no prompt to draw again; \
                     re-roll the picture it started from"
                ),
            )
        }
        None => {
            return refuse(
                "not_rerollable",
                "this picture was not made by one of this gateway's image models",
            )
        }
    };
    // A picture recorded as `image__edit` on a local row that does not edit
    // was a variation (a card's Vary, or the tool before it offered edit
    // models only), and is varied again: its row must still draw.
    let vary = tool == IMAGE_EDIT
        && facts
            .model
            .as_deref()
            .is_some_and(|m| local_edit_row(state, m) == Some(false));
    let alias = made_with(facts, offer, if vary { IMAGE_GENERATE } else { tool })?;
    let Some(prompt) = facts.prompt.clone() else {
        return refuse("not_rerollable", "this picture's prompt was not recorded");
    };
    let mut params = same_settings(facts, prompt);
    params.size = applied(facts, "size", &facts.width.zip(facts.height));
    if tool == IMAGE_GENERATE {
        return Ok(Plan::Image {
            tool,
            alias,
            action: Action::Generate,
            params,
            parent: src.id,
        });
    }
    // An edit or a variation again: from the picture it started from.
    let input = match facts.parent {
        Some(p) => parent_of(p).await.filter(|a| a.kind == "image"),
        None => None,
    };
    let Some(input) = input else {
        return refuse(
            "source_missing",
            "the picture this edit started from is not in the conversation (it was passed \
             inline, or deleted), so it cannot be edited again",
        );
    };
    // The edit's own output size follows its input, as the first one did.
    params.size = None;
    let image = InputImage {
        mime: input.mime,
        bytes: input.data,
    };
    Ok(Plan::Image {
        tool,
        alias,
        action: if vary {
            Action::Vary { image }
        } else {
            Action::Edit { image }
        },
        params,
        parent: input.id,
    })
}

/// Whether the image row published as `alias` edits; `None` for a name that
/// is no image row (a cloud alias, which edits by its edits route).
fn local_edit_row(state: &SharedState, alias: &str) -> Option<bool> {
    let snap = state.snapshot();
    snap.image_models
        .iter()
        .find(|r| snap.image_public_name(&r.model_id) == alias)
        .map(|r| r.edit)
}

fn vary_or_edit(
    state: &SharedState,
    src: &ChatAttachmentFull,
    req: &MediaActionRequest,
    offer: &Offer,
    facts: &MediaFacts,
) -> Result<Plan, Refusal> {
    // The row decides the form (an instruction edit on an `edit` row, a
    // variation on any other); the card names it, so a mismatch is said.
    // A cloud alias edits by instruction.
    let edits = facts
        .model
        .as_deref()
        .and_then(|m| local_edit_row(state, m))
        .unwrap_or(true);
    // An edit needs a model `image__edit` offers; a variation a local row
    // that still draws.
    let alias = made_with(
        facts,
        offer,
        if edits { IMAGE_EDIT } else { IMAGE_GENERATE },
    )?;
    match (req.action, edits) {
        (MediaAction::Vary, true) => {
            return refuse(
                "wrong_action",
                format!("{alias} edits pictures by instruction: use edit, not vary"),
            )
        }
        (MediaAction::Edit, false) => {
            return refuse(
                "wrong_action",
                format!("{alias} makes variations of a picture: use vary, not edit"),
            )
        }
        _ => {}
    }
    let prompt = req
        .prompt
        .clone()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| facts.prompt.clone());
    let Some(prompt) = prompt else {
        return refuse(
            "prompt_required",
            "say in prompt what to make of the picture",
        );
    };
    let mut params = same_settings(facts, prompt);
    params.strength = match req.action {
        MediaAction::Vary => match req.strength {
            Some(s) if !(0.0..=1.0).contains(&s) => {
                return refuse("bad_request", format!("strength {s} is outside 0 to 1"))
            }
            Some(s) => Some(s),
            None => params.strength,
        },
        _ => None,
    };
    let image = InputImage {
        mime: src.mime.clone(),
        bytes: src.data.clone(),
    };
    // A variation is recorded as `image__edit` too: its row says which it
    // was (`reroll`).
    Ok(Plan::Image {
        tool: IMAGE_EDIT,
        alias,
        action: if edits {
            Action::Edit { image }
        } else {
            Action::Vary { image }
        },
        params,
        parent: src.id,
    })
}
