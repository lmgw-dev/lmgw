//! The Chat's image and audio tools (chat-media-tools design §3.4, §5.2,
//! §9): how an image row wants its prompt written, how its live preview is
//! drawn, and the facts a produced file carries in its attachment's `meta`.
//!
//! The labels here are the ones both the gateway (tool descriptions) and the
//! dashboard (the image editor) show, so the two cannot drift.

use serde::{Deserialize, Serialize};

mod defaults;
mod progress;
pub use defaults::*;
pub use progress::*;

/// How an image model wants its prompt written (design §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PromptStyle {
    /// Plain sentences: FLUX, Z-Image, Qwen-Image, SD 3.5 and the like.
    Natural,
    /// Short comma-separated phrases: SDXL base.
    Keywords,
    /// Booru-style tags: the anime fine-tunes.
    Tags,
}

impl PromptStyle {
    pub const ALL: [Self; 3] = [Self::Natural, Self::Keywords, Self::Tags];

    /// The stored and wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Natural => "natural",
            Self::Keywords => "keywords",
            Self::Tags => "tags",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s.trim())
    }

    /// What a person reads (the image editor's choice).
    pub fn label(self) -> &'static str {
        match self {
            Self::Natural => "Plain sentences",
            Self::Keywords => "Short keyword phrases",
            Self::Tags => "Booru-style tags",
        }
    }

    /// What a chat model reads in a tool description: how to write the
    /// prompt for a row of this style.
    pub fn instruction(self) -> &'static str {
        match self {
            Self::Natural => "write the prompt as plain sentences",
            Self::Keywords => "write the prompt as short comma-separated phrases",
            Self::Tags => "write the prompt as comma-separated booru-style tags",
        }
    }
}

/// How sd-server draws a running job's preview (design §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PreviewMode {
    None,
    Proj,
    Tae,
    Vae,
}

impl PreviewMode {
    pub const ALL: [Self; 4] = [Self::None, Self::Proj, Self::Tae, Self::Vae];

    /// The stored spelling, and sd-server's.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Proj => "proj",
            Self::Tae => "tae",
            Self::Vae => "vae",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s.trim())
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Off",
            Self::Proj => "Quick sketch",
            Self::Tae => "Tiny decoder",
            Self::Vae => "Full decoder",
        }
    }

    /// The sentence the editor shows beside the choice.
    pub fn note(self) -> &'static str {
        match self {
            Self::None => "Progress shows elapsed time only.",
            Self::Proj => "A rough colour sketch from the sampler's state. Costs nothing.",
            Self::Tae => {
                "Needs a TAE file in this row's files. sd-server then also decodes the final \
                 image with it, which lowers its quality."
            }
            Self::Vae => {
                "Decodes every step with the row's VAE: slower, and the extra memory raises the \
                 row's learned peak."
            }
        }
    }
}

/// Where a Chat attachment came from (`AttachmentMeta::source`).
pub const SOURCE_UPLOAD: &str = "upload";
/// Made by an `image__*` or `audio__*` call of a turn.
pub const SOURCE_TOOL: &str = "tool";
/// Made by an image card's action.
pub const SOURCE_ACTION: &str = "action";

/// How a media call's result text begins when its user cancelled it (§5.4):
/// a page tells such a card from a failed one by it.
pub const CANCELLED_BY_USER: &str = "Cancelled by the user";

/// The facts of a produced file, under `meta.media` of its attachment
/// (design §9). Every field is optional: what a call did not have is left
/// out, never guessed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MediaFacts {
    /// The tool that made it: `image__generate`, `audio__speak`, ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The model the call named (`image/z-image-turbo`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The model or alias that answered instead of `model`: its fallback,
    /// for whatever reason it could not answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negative_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cfg: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// A variation's strength.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strength: Option<f64>,
    /// The job's own time, `completed − started` (sd-server's job API).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_ms: Option<u64>,
    /// The call's whole time, loading included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
    /// The attachment this one was made from (a card action's source).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upscaler: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// What the answering route could not take and so did not apply
    /// (`seed`, `steps`, ...): a cloud fallback gets no smuggled parameters.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_applied: Vec<String>,
}

/// What an image card's action does (design §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MediaAction {
    /// The same prompt and settings, a new seed.
    Reroll,
    /// A variation (img2img) of the picture, on a row without `edit`.
    Vary,
    /// An instruction edit of the picture, on a row with `edit`.
    Edit,
    /// An ESRGAN upscale of the picture.
    Upscale,
}

impl MediaAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reroll => "reroll",
            Self::Vary => "vary",
            Self::Edit => "edit",
            Self::Upscale => "upscale",
        }
    }
}

/// `POST /chat/api/attachments/{id}/media` (design §8.3): run one action on
/// a picture of the thread. Every field but `action` is optional; what a
/// field does not say is the source picture's own (its prompt, its strength)
/// or the server's (the upscaler, one repeat).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MediaActionRequest {
    pub action: MediaAction,
    /// `vary` and `edit`: the prompt, the source's own when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub prompt: Option<String>,
    /// `vary`: how far to move from the picture, 0 to 1; the source's own
    /// when omitted, else the row's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub strength: Option<f64>,
    /// `upscale`: the upscaler's name (a file stem of the image models dir's
    /// `upscalers/`); the server's first compatible one when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub upscaler: Option<String>,
    /// `upscale`: how many times to run it, 1 to 4 (sd-server's bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub repeats: Option<u32>,
}

/// The action stream's first frame (`event: action`): the id the action
/// runs under — what its progress frames carry as `call_id`, and what the
/// media cancel route takes — and the model it runs on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MediaActionStarted {
    /// `act-<boot>-<n>`: `<boot>` 32 random bits (eight hex digits) drawn
    /// once per gateway process, `<n>` a counter, so a later process repeats
    /// an id only if it draws the same bits.
    pub call_id: String,
    /// `reroll`, `vary`, `edit` or `upscale`.
    pub action: String,
    /// The model the action runs on (`image/z-image-turbo`); absent for an
    /// upscale, whose row the gateway picks when it runs (its `done` names
    /// it).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub model: Option<String>,
    /// The attachment the action started from.
    pub source: i64,
}

/// The action's last frame on success (`event: done`): the picture it made,
/// stored as the thread's attachment (`source = "action"`, bound to the
/// source's message, `meta.media.parent` the source), and a sentence about
/// it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MediaActionDone {
    pub call_id: String,
    pub attachment: crate::chat_threads::AttachmentMeta,
    /// What happened, in words: `Made image att:58 (1024x1024) with …`.
    pub text: String,
}

/// The events the action stream sends, in the order they may come.
pub const MEDIA_ACTION_EVENTS: &[&str] = &["action", "progress", "done", "error"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_action_body_reads_with_only_its_action() {
        let r: MediaActionRequest =
            serde_json::from_value(serde_json::json!({"action": "upscale"})).unwrap();
        assert_eq!(r.action, MediaAction::Upscale);
        assert_eq!(r.repeats, None);
        assert!(serde_json::from_value::<MediaActionRequest>(
            serde_json::json!({"action": "paint"})
        )
        .is_err());
    }

    #[test]
    fn the_spellings_round_trip() {
        for p in PromptStyle::ALL {
            assert_eq!(PromptStyle::parse(p.as_str()), Some(p));
            let wire = serde_json::to_value(p).unwrap();
            assert_eq!(wire, serde_json::json!(p.as_str()));
        }
        for p in PreviewMode::ALL {
            assert_eq!(PreviewMode::parse(p.as_str()), Some(p));
            let wire = serde_json::to_value(p).unwrap();
            assert_eq!(wire, serde_json::json!(p.as_str()));
        }
        assert_eq!(PromptStyle::parse("prose"), None);
    }

    #[test]
    fn facts_leave_out_what_a_call_did_not_have() {
        let f = MediaFacts {
            tool: Some("image__generate".into()),
            seed: Some(7),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            serde_json::json!({"tool": "image__generate", "seed": 7})
        );
        let back: MediaFacts = serde_json::from_value(serde_json::json!({"seed": 7})).unwrap();
        assert_eq!(back.seed, Some(7));
    }
}
