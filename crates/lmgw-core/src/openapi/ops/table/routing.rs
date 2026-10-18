//! `ops-routing`: upstream and alias plumbing (api-docs design §4.7).

use lmgw_api_types as dto;

use super::{OpArgs, OpDoc, Resp};
use crate::openapi::ops::args;

const TAG: &str = "ops-routing";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "upstream_set",
        tag: TAG,
        summary: "Create, update, delete, enable or disable an upstream provider",
        description: None,
        tool: Some("lmgw__upstream_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::UpstreamPatch>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::RowSaved>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "model_set",
        tag: TAG,
        summary: "Create, update, delete, enable or disable a model alias",
        description: None,
        tool: Some("lmgw__model_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::AliasPatch>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::RowSaved>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "candidate_alias_set",
        tag: TAG,
        summary: "Create, update, delete, enable, disable or preview a candidate alias",
        description: Some(
            "The answer follows the action: `preview` answers CandidateAliasPreview (the \
             draft's derivation and the models it could add, written nowhere), every other \
             action CandidateAliasSaved.",
        ),
        tool: Some("lmgw__candidate_alias_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::CandidateAliasPatch>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::CandidateAliasAnswer>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "alias_set",
        tag: TAG,
        summary: "Create or update a model alias, including parameter overrides",
        description: Some(
            "Alias create/update including param overrides, which model_set does not carry \
             (its tool-plane schema stays scalar-only). Overrides are replaced wholesale — the \
             form always sends the complete set.",
        ),
        tool: None,
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::web::api::AliasFullPatch>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::RowSaved>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "upstream_set_full",
        tag: TAG,
        summary: "Create or update an upstream provider, including extra headers",
        description: Some(
            "Upstream create/update including extra_headers, which the tool-plane patch omits. \
             Secrets follow the house convention: an empty api_key keeps the stored one, and a \
             header value of \"<set>\" round-trips to the stored value for that header name.",
        ),
        tool: None,
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::web::api::UpstreamFullPatch>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::RowSaved>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "model_visibility",
        tag: TAG,
        summary: "Hide or unhide models of an expose-all upstream's live catalog",
        description: Some(
            "Hide/unhide one or more models of an `expose_all` upstream's live catalog from \
             /v1/models and the client-facing model list. The model itself is never configured \
             here — it is whatever the upstream's catalog reports — so there is no `id`, only \
             the (upstream_id, model_id) pair that names one entry of a live catalog.",
        ),
        tool: None,
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::web::api::ModelVisibilityPatch>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::ModelVisibilitySaved>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "upstream_test",
        tag: TAG,
        summary: "Test connectivity to a configured upstream",
        description: Some(
            "Connect to one configured upstream now. It answers `{ok: true}` when the model \
             list came back; a failed connection is an error answer with the upstream's own \
             words.",
        ),
        tool: None,
        args: OpArgs::Hand(args::upstream_test),
        response: Resp::Json(|g| g.root_schema_for::<dto::UpstreamTested>()),
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
];
