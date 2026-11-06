//! `ops-runtime`: the per-model container runtime, the GPU hold, the
//! machine check (machine-check design §4.2), and background jobs (api-docs
//! design §4.7).

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
