//! `ops-runtime`: the per-model container runtime, the GPU hold, and
//! background jobs (api-docs design §4.7).

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
