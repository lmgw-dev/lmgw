//! `ops-settings`: gateway settings and the update check (api-docs design
//! §4.7).

use super::{OpArgs, OpDoc, Resp};

const TAG: &str = "ops-settings";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "settings_set",
        tag: TAG,
        summary: "Change gateway settings",
        description: None,
        tool: Some("lmgw__settings_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::SettingsPatch>()),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "settings_set_full",
        tag: TAG,
        summary: "Change gateway settings, including the fields the self-admin tool excludes",
        description: Some(
            "Sparse patch over the whole Settings struct — the dashboard's Settings page save, \
             including the self-admin mode, the bind address, the HF/update/forge tokens, the \
             builds directory and the four container classes' definitions, none of which the \
             self-admin tool plane exposes. Secrets: a non-empty value replaces, an empty one \
             keeps the stored value, and the matching clear_* flag erases it. A change of \
             self_admin applies to every paired device at once, since it caps each device's \
             own level: a device whose admin tools may now do more or less gets a state event \
             on its change feed saying so, its realtime sessions that carry the lmgw label \
             list it again, and a turn still running is refused its next write tool when the \
             level was lowered. Set to off, every device stops seeing the Chat threads and \
             folders that carry the self-admin toolset: its change feed receives them as \
             deleted, its turns on them stop, and its realtime sessions bound to one close \
             with 4004; raised from off, a device whose own level is above off receives them \
             as created.",
        ),
        tool: None,
        args: OpArgs::Struct(|g| {
            g.root_schema_for::<crate::web::api_settings::SettingsFullPatch>()
        }),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "realtime_budget",
        tag: TAG,
        summary: "Size the realtime voice cascade against the GPU",
        description: Some(
            "What the configured GET /v1/realtime cascade is expected to hold on the GPU, stage \
             by stage — turn detection (CPU), the chat model, speech to text, the barge-in word \
             check when it uses another model, text to speech — with their sum, the VRAM \
             headroom, what lmgw may use (vram.budget_mb or the GPU's total) and what other \
             programs hold now. Each figure is the one admission charges: a chat row's GGUF \
             weights + KV cache, an audio row's learned residency or, before it has one, its \
             on-disk size; a cloud stage holds nothing, and a stage with no figure is listed \
             as unknown. An argument left out is the saved setting, so a draft can be sized \
             before it is saved. Starts nothing.",
        ),
        tool: None,
        args: OpArgs::Struct(|g| {
            g.root_schema_for::<lmgw_api_types::realtime::RealtimeBudgetArgs>()
        }),
        response: Resp::Json(|g| g.root_schema_for::<lmgw_api_types::realtime::RealtimeBudget>()),
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "update_check",
        tag: TAG,
        summary: "Check whether a newer lmgw release is available",
        description: Some(
            "Poll the package registry now for a newer lmgw release than the one running, \
             regardless of the periodic update-check setting.",
        ),
        tool: None,
        args: OpArgs::NoArgs,
        response: Resp::OpOutcome,
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
];
