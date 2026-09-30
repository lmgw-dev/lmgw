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
             keeps the stored value, and the matching clear_* flag erases it.",
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
