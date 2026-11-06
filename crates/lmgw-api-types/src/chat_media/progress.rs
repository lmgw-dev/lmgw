//! A media call while it runs (chat-media-tools design §5, §6.2, §6.5): the
//! `progress` frame of the Chat's `tool` event, and what the per-call
//! cancel route answers.

use serde::{Deserialize, Serialize};

/// What a running image or audio call is doing (`ToolProgressFrame::phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MediaPhase {
    /// The call's model's engine image is being downloaded before the model
    /// can be loaded (machine-check design §5): `image`, `done_bytes`, and
    /// `total_bytes` where the size is known.
    Downloading,
    /// Another model is being stopped to make room: `model` names it.
    Unloading,
    /// The call's own model is being brought onto the GPU.
    Loading,
    /// Nothing more can be stopped yet: the call waits for memory
    /// (`needs_bytes`, `free_bytes`).
    WaitingForRoom,
    /// The image server holds the job in its queue (`queue_position`).
    Queued,
    /// The picture is being drawn (`pass`, `step`, `total_steps`, `preview`
    /// where the image server reports them).
    Generating,
    Speaking,
    Transcribing,
    Upscaling,
}

impl MediaPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Downloading => "downloading",
            Self::Unloading => "unloading",
            Self::Loading => "loading",
            Self::WaitingForRoom => "waiting_for_room",
            Self::Queued => "queued",
            Self::Generating => "generating",
            Self::Speaking => "speaking",
            Self::Transcribing => "transcribing",
            Self::Upscaling => "upscaling",
        }
    }
}

/// What cancelling a running image job does (design §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum CancelKind {
    /// The image server's own cancel: the job is dropped, the model stays.
    Native,
    /// The image server cannot stop a running picture, and this call is the
    /// model's only request: lmgw stops its container, and the next picture
    /// loads it again.
    Restart,
    /// The image server cannot stop a running picture, and another request
    /// uses the model: the call stops waiting, the picture finishes in the
    /// background and is thrown away.
    Abandon,
}

impl CancelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Restart => "restart",
            Self::Abandon => "abandon",
        }
    }
}

/// The `progress` event of a `tool` frame: a running image or audio call's
/// stage, step and latest preview. Lossy: a reader that falls behind misses
/// some (the next one supersedes them), and they are not in the change
/// feed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolProgressFrame {
    /// The call's id, as its `ready` frame carries it.
    pub call_id: String,
    pub phase: Option<MediaPhase>,
    /// The model the phase is about: the one being unloaded for
    /// `unloading`, the call's own otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub model: Option<String>,
    /// The sampling pass (a batch item, a high- or low-noise stage, a hires
    /// pass); a new pass starts its steps again.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub pass: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub step: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub total_steps: Option<u32>,
    /// `queued`: the job's place in the image server's queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub queue_position: Option<u32>,
    /// `downloading`: the image reference being downloaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub image: Option<String>,
    /// `downloading`: compressed bytes downloaded so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub done_bytes: Option<u64>,
    /// `downloading`: the download's whole size; absent when it is not
    /// known.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub total_bytes: Option<u64>,
    /// `downloading`: why the size is not known, when it is not ("size not
    /// known — …").
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub size_note: Option<String>,
    /// `waiting_for_room`: the memory the model needs, headroom included.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub needs_bytes: Option<u64>,
    /// `waiting_for_room`: the memory free when it was last measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub free_bytes: Option<u64>,
    /// Since the call started.
    pub elapsed_ms: u64,
    /// What a cancel would do now, while an image job runs; absent before
    /// the job exists (a cancel then just stops the wait) and for audio.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub cancel: Option<CancelKind>,
    /// The latest preview as the image server drew it: base64, PNG. Its
    /// pixel size depends on the row's preview mode (a quick sketch is the
    /// latent's size, an eighth of the picture's side on the spike's
    /// pipeline).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub preview: Option<String>,
}

/// The per-call cancel route's path (`POST`, design §6.5).
pub const MEDIA_CANCEL_PATH: &str = "/chat/api/threads/{id}/media/{call_id}/cancel";

/// What `POST /chat/api/threads/{id}/media/{call_id}/cancel` did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MediaCancelled {
    pub ok: bool,
    /// What the cancel does to the running image job; absent when no job
    /// runs yet (the call was still loading its model or waiting for room,
    /// and stops waiting) and for an audio call.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub cancel: Option<CancelKind>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_progress_frame_leaves_out_what_its_phase_has_not() {
        let f = ToolProgressFrame {
            call_id: "call_3".into(),
            phase: Some(MediaPhase::Generating),
            model: Some("image/z-image-turbo".into()),
            pass: Some(1),
            step: Some(3),
            total_steps: Some(8),
            elapsed_ms: 2140,
            cancel: Some(CancelKind::Restart),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            json!({"call_id": "call_3", "phase": "generating", "model": "image/z-image-turbo",
                   "pass": 1, "step": 3, "total_steps": 8, "elapsed_ms": 2140,
                   "cancel": "restart"})
        );
        for p in [
            MediaPhase::WaitingForRoom,
            MediaPhase::Unloading,
            MediaPhase::Downloading,
        ] {
            assert_eq!(serde_json::to_value(p).unwrap(), json!(p.as_str()));
        }
        for c in [CancelKind::Native, CancelKind::Restart, CancelKind::Abandon] {
            assert_eq!(serde_json::to_value(c).unwrap(), json!(c.as_str()));
        }
    }
}
