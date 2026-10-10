//! `ops-models`: the local model classes — chat, aux (embed/rerank), audio,
//! image — and the load smoke test (api-docs design §4.7).

use lmgw_api_types as dto;

use super::{OpArgs, OpDoc, Resp};

const TAG: &str = "ops-models";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "local_model_set",
        tag: TAG,
        summary: "Create, update, delete, enable, disable or duplicate a local chat model",
        description: None,
        tool: Some("lmgw__local_model_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::LocalModelPatch>()),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "aux_model_set",
        tag: TAG,
        summary: "Create, update, delete, enable or disable an aux (embedding/rerank) model",
        description: None,
        tool: Some("lmgw__aux_model_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::AuxModelPatch>()),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "embed_model_set",
        tag: TAG,
        summary: "Deprecated spelling of aux_model_set",
        description: Some(
            "The pre-rename spelling of aux_model_set, still accepted so a browser holding an \
             older SPA bundle keeps working. New callers should use aux_model_set.",
        ),
        tool: None,
        args: OpArgs::AliasOf("aux_model_set"),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: true,
        example: None,
    },
    OpDoc {
        name: "audio_model_set",
        tag: TAG,
        summary: "Create, update, delete, enable or disable an audio.cpp model",
        description: Some(
            "Audio model CRUD — the same shape as aux_model_set (sparse patch, unknown fields \
             rejected, the snapshot reloaded after every write, apply stays a separate step), \
             adapted to the wider audio.cpp field set (task, mode, load/session/request \
             options, voice presets, and where the row runs: backend 'cpu' with its threads, \
             which takes no VRAM and keeps serving under the GPU hold). The same function \
             lmgw__audio_model_set calls.",
        ),
        tool: Some("lmgw__audio_model_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::AudioPatch>()),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "voice_transcribe",
        tag: TAG,
        summary: "Transcribe voice-library clips with a speech-to-text model",
        description: Some(
            "Writes a voice-library clip's transcript (the library's prompt_text, which \
             audio.cpp hands a cloning model as reference_text when a request's voice names \
             the clip): one clip (replaced if it had one), or every clip without one. The \
             model is the alias given, else the setting audio.voice_transcribe_alias; it must \
             be a speech-to-text (asr) model, wherever it runs, and so must the fallback a GPU \
             hold or an outside-VRAM verdict hands the clip to (a configured fallback is always \
             used; asr_required before anything is sent otherwise). The answer names the clips, \
             their transcript lengths and the model that wrote each (a fallback named as one: \
             answered_by, fallback_reason), not the text. The same function \
             lmgw__voice_transcribe calls.",
        ),
        tool: Some("lmgw__voice_transcribe"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::audio_lab::VoiceTranscribed>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: Some(r#"{"alias":"audio/qwen3-asr"}"#),
    },
    OpDoc {
        name: "image_model_set",
        tag: TAG,
        summary: "Create, update, delete, enable or disable an image (stable-diffusion.cpp) model",
        description: None,
        tool: Some("lmgw__image_model_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::ImageModelPatch>()),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "local_model_test",
        tag: TAG,
        summary: "Load a configured local model and prove it does its job",
        description: None,
        tool: Some("lmgw__local_model_test"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::ModelTest>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
];
