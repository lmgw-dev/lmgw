//! `ops-models`: the local model classes — chat, aux (embed/rerank), audio,
//! image — and the load smoke test (api-docs design §4.7).

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
             options, voice presets).",
        ),
        tool: None,
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::web::api::AudioPatch>()),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
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
        response: Resp::Untyped(
            "the shape depends on the model's class (chat/aux/image) and whether it is a \
             ladder — see lmgw__local_model_test's description",
        ),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
];
