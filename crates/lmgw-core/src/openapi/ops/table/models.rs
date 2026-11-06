//! `ops-models`: the local model classes — chat, aux (embed, rerank,
//! decision), audio, image — and the load smoke test (api-docs design §4.7).

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
        response: Resp::Json(|g| g.root_schema_for::<dto::LocalModelSaved>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "aux_model_set",
        tag: TAG,
        summary: "Create, update, delete, enable or disable an aux (embedding, rerank or \
                  decision) model",
        description: None,
        tool: Some("lmgw__aux_model_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::AuxModelPatch>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::AuxModelSaved>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
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
        response: Resp::Json(|g| g.root_schema_for::<dto::AudioModelSaved>()),
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
        name: "voice_generate",
        tag: TAG,
        summary: "Render candidate voice samples with a text-to-speech model (the Voices lab)",
        description: Some(
            "One round of the Voices lab: the alias (any text-to-speech model, local or cloud) \
             is opened once and says the same text as `count` candidates (4 when absent; no \
             cap, rendered one after another), each with a seed of its own — consecutive from \
             `seed`, else drawn — which reaches a local row whose engine reads one \
             (`seeds_sent`). The text is `text`, else the language's sentence (the owner's \
             saved one, else the built-in one). `description` is the voice description or \
             speaking style (the speech route's instructions), `voice` the voice for a model \
             that speaks one it is told; `language` goes to a local row that takes one. A \
             configured fallback answers as for any request, and the round names who \
             answered (answered_by, fallback_reason). The round is kept under \
             <data_dir>/voice-lab/rounds until discarded; each candidate's `url` serves its \
             audio. A failure after the first candidate ends the round there (`stopped`). The \
             same function lmgw__voice_generate and POST /voice-lab/api/rounds call.",
        ),
        tool: Some("lmgw__voice_generate"),
        args: OpArgs::Struct(|g| g.root_schema_for::<dto::voice_lab::VoiceGenerateArgs>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::voice_lab::VoiceRound>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: Some(
            r#"{"alias":"audio/voicedesign","language":"en","description":"a warm, calm voice","count":4}"#,
        ),
    },
    OpDoc {
        name: "voice_sample_save",
        tag: TAG,
        summary: "Save a Voices lab candidate to the voice library as a sample",
        description: Some(
            "Writes a round's candidate to the voice library (<audio models dir>/voices) as \
             <name>.wav, its leading and trailing silence cut (loudness untouched), with the \
             text it says as its transcript (the library's prompt_text) — so every cloning \
             model can speak with it by name. How it was made (alias, the model that \
             answered, description, voice, seed, text, language, dates, licence when known) \
             is recorded beside it. A name the library already has is refused with \
             `name_taken` and nothing is written, unless `overwrite` is true: then the clip of \
             that name is replaced. The same function lmgw__voice_sample_save and POST \
             /voice-lab/api/samples call.",
        ),
        tool: Some("lmgw__voice_sample_save"),
        args: OpArgs::Struct(|g| g.root_schema_for::<dto::voice_lab::VoiceSampleSaveArgs>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::voice_lab::VoiceSampleSaved>()),
        writes: true,
        reveals_secret: false,
        confirm_note: Some("A sample of the same name is replaced only with overwrite."),
        deprecated: false,
        example: Some(
            r#"{"round":"20260101-120000-000-abcdef","candidate":2,"name":"sample-voice"}"#,
        ),
    },
    OpDoc {
        name: "image_model_set",
        tag: TAG,
        summary: "Create, update, delete, enable or disable an image (stable-diffusion.cpp) model",
        description: None,
        tool: Some("lmgw__image_model_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::ImageModelPatch>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::ImageModelSaved>()),
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
