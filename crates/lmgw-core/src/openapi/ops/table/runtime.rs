//! `ops-runtime`: the per-model container runtime, the GPU hold, the
//! machine check (machine-check design §4.2), the document tools found on
//! this computer (document-conversion design §7.5), and background jobs
//! (api-docs design §4.7).

use lmgw_api_types as dto;

use super::{OpArgs, OpDoc, Resp};
use crate::openapi::ops::args;

const TAG: &str = "ops-runtime";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "container",
        tag: TAG,
        summary: "Control the per-model container runtime",
        description: None,
        tool: Some("lmgw__container"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<lmgw_api_types::ContainerAnswer>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "hold_set",
        tag: TAG,
        summary: "Engage or release the GPU hold",
        description: Some(
            "Engaging stops every idle local container (`stopped`), leaves the ones still \
             working to the next reaper tick (`draining`), names the stops that failed \
             (`failed`) and aborts a benchmark run in flight (`benchmark_aborted`, absent when \
             none was). Asking for the state the hold is already in is not an error: nothing \
             changes, and the answer reports the current state.",
        ),
        tool: Some("lmgw__hold_set"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::HoldSet>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "headroom_forget",
        tag: TAG,
        summary: "Forget the headroom's readings of other programs' GPU memory",
        description: Some(
            "Auto headroom is kept per graphics card from the readings of other programs' GPU \
             memory there. With `card` (a card's id, as `HeadroomNow.cards` lists it) only that \
             card's readings are deleted, in memory and stored: until its next reading that card \
             keeps the whole `headroom_mb` as its floor and no learned room; the other cards keep \
             theirs. Without `card` every card's readings go. For a most a one-off taught, such \
             as a game run without the hold, or a card that changed hands. A card that is \
             neither listed nor has readings is refused. The audit log names the card and the \
             figure that was dropped. The answer is the headroom as it is now (`HeadroomNow`).",
        ),
        tool: None,
        args: OpArgs::Struct(|g| g.root_schema_for::<dto::HeadroomForgetArgs>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::HeadroomNow>()),
        writes: true,
        reveals_secret: false,
        confirm_note: Some(
            "forgets the readings of other programs' GPU memory, of one card or of every card; \
             auto headroom keeps the whole fixed value on that card until its next reading",
        ),
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "machine_check",
        tag: TAG,
        summary: "Check whether this computer can run local models",
        description: None,
        tool: Some("lmgw__machine_check"),
        args: OpArgs::Struct(|g| g.root_schema_for::<dto::machine::MachineCheckArgs>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::machine::MachineCheck>()),
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "machine_settings_fit",
        tag: TAG,
        summary: "Switch the model settings to the ones for this computer's graphics card",
        description: Some(
            "Switches each model class's image and extra podman run arguments (and the audio \
             class's backend) to the values lmgw ships for the card `fit` names: \
             `amd_vulkan` (llama.cpp's, audio.cpp's and stable-diffusion.cpp's Vulkan images, \
             `--device /dev/dri --group-add keep-groups --security-opt label=disable`) or \
             `nvidia_cuda` (lmgw's defaults). Only a class set for the other card is \
             switched; one on the processor, one naming another CDI device and one giving \
             models the render nodes with an image that is not lmgw's for AMD cards are left \
             as they are (`left` says why). Run arguments, a class's and a model's own alike, \
             keep every argument but the other card's flags, which are swapped for this card's \
             (none for a model on the processor); arguments that would add the user's groups \
             beside a group of their own are left, since Podman refuses the two together. A \
             model's own image that is one of the engines' published images for the other card \
             becomes this card's; either is cleared when it then equals its class's, so the \
             model follows its class again. An image lmgw can tell is built for the other card \
             by its build label or its libraries, and one it cannot tell, stay, named in \
             `left`. Without `confirm` nothing is written and the answer lists what would \
             change and the running models it concerns, as the machine check's settings item \
             does (its `action`). `confirm` is that list of `changes`, as it was shown: the \
             switch is worked out again under the settings lock and written only when it is \
             the same list, the classes through the Settings page's own save and the models' \
             settings in its transaction, all or nothing; otherwise nothing is written and the \
             answer's `outdated` says what differs, with the switch as it is now to confirm \
             again. After the write each running model it concerns is stopped (never forced: \
             one answering a request keeps running with the settings from before, and the \
             answer and the machine check say so until it stops), and the answer carries the \
             machine check run again. Refused when this computer has no card of that maker. \
             No password: it is an ordinary settings write.",
        ),
        tool: None,
        args: OpArgs::Struct(|g| g.root_schema_for::<dto::machine::MachineSettingsFitArgs>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::machine::MachineSettingsFit>()),
        writes: true,
        reveals_secret: false,
        confirm_note: Some(
            "changes the image and run arguments of every model class set for the other card, \
             and the models' own ones that follow them, then stops the running models whose \
             settings changed",
        ),
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "doc_tools",
        tag: TAG,
        summary: "What this computer can convert to text, and what installing a tool would add",
        description: None,
        tool: Some("lmgw__doc_tools"),
        args: OpArgs::NoArgs,
        response: Resp::Json(|g| g.root_schema_for::<dto::documents::DocCaps>()),
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "doc_jobs_stored",
        tag: TAG,
        summary: "What background document conversions hold on disk and still have queued",
        description: Some(
            "Settings > Retention shows it beside the retention of conversion jobs (document \
             conversion design §7.3): jobs with files in the data dir, the bytes those take \
             (uploads of running jobs, converted text of ended ones until the retention removes \
             them), jobs running now and the files of running jobs that have no outcome yet. \
             Jobs keep their uploads on disk while queued, so this is where a queue that grows \
             shows.",
        ),
        tool: Some("lmgw__doc_jobs_stored"),
        args: OpArgs::NoArgs,
        response: Resp::Json(|g| g.root_schema_for::<dto::documents::jobs::JobsStored>()),
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "doc_adapter_usage",
        tag: TAG,
        summary: "Who calls the docling-serve-compatible routes, and what they send",
        description: Some(
            "Summed per key from the convert_calls records beside the request log (document \
             conversion §7.8): requests, files, last seen, the /v1/convert/* routes with \
             counts, each docling option set with its value and outcome (honoured, mapped, \
             not_yet, refused; the ones other than honoured first), and the hosts of http \
             sources. Option values are kept for enum-like options only; free text, addresses, \
             headers, prompts, file names and content are never recorded. The records follow \
             the request log's retention, so the summary needs no store of its own.",
        ),
        tool: Some("lmgw__doc_adapter_usage"),
        args: OpArgs::NoArgs,
        response: Resp::Json(|g| {
            g.root_schema_for::<dto::documents::adapter_usage::AdapterUsage>()
        }),
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "doc_model_add",
        tag: TAG,
        summary: "Download a model file document conversion can use",
        description: Some(
            "Queues one of the model files doc_tools lists under models (today the layout \
             model, id 'layout': docling's heron layout model as its own ONNX export, \
             Apache-2.0, about 163 MiB) into the folder aux model files go into, at \
             <owner>/<repo>/<file>, through the download queue: it shows on the Downloads \
             page, and hf_downloads with its id follows it. The file is taken at the commit \
             lmgw pins; the hub's listing must name the pinned SHA-256 or nothing is queued, \
             and the gateway checks the bytes against it again before the first load. A file \
             already there or already on its way is not queued again. A download loads \
             nothing and switches nothing on: settings_set documents_onnx_layout=true does, \
             once the file is here. Refused when the aux models dir is not configured.",
        ),
        tool: Some("lmgw__doc_model_add"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::documents::models::DocModelAddResult>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "doc_tools_check",
        tag: TAG,
        summary: "Check the optional document tools again",
        description: Some(
            "Settings › Documents' Check again: every optional tool (poppler, tesseract, \
             pandoc, ffmpeg, LibreOffice) is looked up on PATH (lmgw's own document reader \
             where lmgw installs it, never on PATH) and, where found, run for its \
             version (and, by tool, its language packs, audio decoders or installed parts) in \
             a private empty working directory, each run stopped after TOOL_PROBE_TIMEOUT \
             (10 s), which a failed probe's message names. A check that began after this call \
             is joined rather than run twice. The answer is the same as doc_tools'. It only \
             reads: nothing is installed or stored.",
        ),
        tool: None,
        args: OpArgs::NoArgs,
        response: Resp::Json(|g| g.root_schema_for::<dto::documents::DocCaps>()),
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "job_cancel",
        tag: TAG,
        summary: "Cancel a running background job",
        description: Some(
            "Cancel a running background job (a download, a build run, an image pull, …) by \
             its job id.",
        ),
        tool: None,
        args: OpArgs::Hand(args::job_cancel),
        response: Resp::Json(|g| g.root_schema_for::<dto::MessageAck>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
];
