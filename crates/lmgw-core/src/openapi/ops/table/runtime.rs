//! `ops-runtime`: the per-model container runtime, the GPU hold, and
//! background jobs (api-docs design §4.7).

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
        response: Resp::Untyped(
            "the shape depends on `action` (status/start/stop/restart/apply/logs) and whether \
             `target`/`model` names one model or a whole class — see lmgw__container's \
             description",
        ),
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
        description: None,
        tool: Some("lmgw__hold_set"),
        args: OpArgs::Tool,
        response: Resp::OpOutcome,
        writes: true,
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
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
];
