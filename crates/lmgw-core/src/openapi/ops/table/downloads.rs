//! `ops-downloads`: Hugging Face downloads, the image-recipe shortcuts, and
//! the audio.cpp catalog (api-docs design §4.7).

use lmgw_api_types as dto;

use super::{OpArgs, OpDoc, Resp};
use crate::openapi::ops::args;

const TAG: &str = "ops-downloads";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "hf_add",
        tag: TAG,
        summary: "Download a model from Hugging Face into a models directory",
        description: None,
        tool: Some("lmgw__hf_add"),
        args: OpArgs::Tool,
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "hf_set",
        tag: TAG,
        summary: "Re-fetch, cancel, untrack or check-updates on a tracked Hugging Face download",
        description: None,
        tool: Some("lmgw__hf_set"),
        args: OpArgs::Tool,
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "image_recipes",
        tag: TAG,
        summary: "List the image pipelines lmgw ships a recipe for",
        description: None,
        tool: Some("lmgw__image_recipes"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::ImageRecipes>()),
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "image_recipe_add",
        tag: TAG,
        summary: "Queue every missing component of one image recipe",
        description: None,
        tool: Some("lmgw__image_recipe_add"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::ImageRecipeAddResult>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "audio_catalog",
        tag: TAG,
        summary: "Refresh the audio.cpp model catalog, or queue one package's download",
        description: Some(
            "The two catalog actions of the Audio lab, as one op: 'refresh' live-fetches \
             audio.cpp's model_specs catalog, lists each package repo on Hugging Face once \
             (which spec files are published — a package's unpublished_files), and persists \
             the snapshot (the only network calls in this domain; a spec file or repo listing \
             that fails is one of its warnings, not a failed refresh); 'download' queues the files one package lacks (family + \
             package) through the shared Hugging Face download queue, target audio — every \
             file for a package never downloaded, only the missing ones for one the spec grew \
             since (the catalog's missing_files; 'complete install' in the Audio catalog). \
             The same function lmgw__audio_catalog calls.",
        ),
        tool: Some("lmgw__audio_catalog"),
        args: OpArgs::Hand(args::audio_catalog),
        response: Resp::Json(|g| g.root_schema_for::<dto::AudioCatalogAnswer>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: Some(r#"{"action":"download","family":"kokoro","package":"kokoro-82m"}"#),
    },
];
