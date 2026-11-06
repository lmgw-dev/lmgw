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
        response: Resp::Json(|g| g.root_schema_for::<dto::HfAddResult>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "model_get",
        tag: TAG,
        summary: "Get a model from Hugging Face and set it up, sized to the graphics card",
        description: None,
        tool: Some("lmgw__model_get"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::model_get::ModelGetStarted>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "model_get_status",
        tag: TAG,
        summary: "How a Get it stands, and the model it set up",
        description: Some(
            "One get by the `get_id` `model_get` answered (find-a-model design §6.2): its state \
             (running, done, failed, canceled), a running get's stage (downloading, paused, \
             setting up, ready) with each file that waits and why (`waiting`: `metered`, \
             `not_started`, or `other` with the download's note) and its bytes so far, every \
             file's download row, why a failed get stopped and what to do, and a finished get's result: the \
             row it created or found, its context, the fit that context comes from and the \
             note the row keeps. Reads only.",
        ),
        tool: Some("lmgw__model_get_status"),
        args: OpArgs::Struct(|g| g.root_schema_for::<dto::model_get::ModelGetStatusArgs>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::model_get::ModelGetStatus>()),
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "model_fit_again",
        tag: TAG,
        summary: "Fit a chat or aux model's context to the graphics card again",
        description: Some(
            "The model editor's \"Fit again\" (find-a-model design D26): the fit of the row's \
             own parameters on the graphics card as it is now, searched up to the model's \
             trained context, with the row itself left out of what is beside it. The measure \
             is the one the row's fit note records, else beside what runs now. Without \
             `confirm` nothing is written and the answer lists the `changes` a save makes: the \
             context, and for an aux row an ubatch and a batch it ties to the context. \
             `confirm` is that list as it was shown: the fit is taken again, and the list is \
             saved when the context it sets still fits the card now and the row's parameters \
             are still the ones it was made from; else nothing is written and `outdated` is \
             true with the new changes. A save writes the row through its class's own update \
             and renews the row's fit note (none when the whole trained context fits). \
             Nothing is started. The note itself is on the row's read (`fit_note`).",
        ),
        tool: None,
        args: OpArgs::Struct(|g| g.root_schema_for::<dto::model_get::ModelFitAgainArgs>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::model_get::ModelFitAgain>()),
        writes: true,
        reveals_secret: false,
        confirm_note: Some(
            "changes the model's context, and an aux model's ubatch and batch tied to it",
        ),
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "hf_set",
        tag: TAG,
        summary: "Re-fetch, cancel, untrack or check-updates on a tracked Hugging Face download",
        description: Some(
            "Manage tracked Hugging Face downloads: re-fetch one, cancel a running transfer, \
             untrack and delete its file, or ETag-check every completed download against the \
             hub for updates. An audio catalog file is checked and re-fetched at the revision \
             audio.catalog_revision takes it at now: under 'pinned' the commit its audio.cpp \
             spec pins today, else main. The answer follows the action: `check_updates` answers \
             HfUpdatesChecked (how many finished downloads of the `target` class differ from \
             the hub, and one sentence per file it could not ask about, in which case `ok` is \
             false); `redownload`, `cancel` and `delete` answer a MessageAck.",
        ),
        tool: Some("lmgw__hf_set"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::HfSetAnswer>()),
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
        name: "image_upscaler_add",
        tag: TAG,
        summary: "Queue a shipped ESRGAN upscaler into the image models dir's upscalers/",
        description: Some(
            "Downloads one of the upscalers image_recipes lists under upscalers.shipped into \
             upscalers/<file> of the image models dir — not the <owner>/<repo>/<file> layout \
             of hf_add — because sd-server finds upscalers at the top level of that one \
             directory, which every image row reads. A file already there or already \
             downloading is not queued again. An image model's server sees the file from its \
             next start; image__upscale and the Chat card's Upscale then offer it by its file \
             stem. Refused when the image models dir is not configured.",
        ),
        tool: Some("lmgw__image_upscaler_add"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::ImageUpscalerAddResult>()),
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
    OpDoc {
        name: "model_find",
        tag: TAG,
        summary: "Find a model on Hugging Face by its name, sized to this computer's card",
        description: None,
        tool: Some("lmgw__model_find"),
        args: OpArgs::Struct(|g| g.root_schema_for::<dto::model_find::ModelFindArgs>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::model_find::ModelFind>()),
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: Some(r#"{"query":"qwen 3.6"}"#),
    },
];
