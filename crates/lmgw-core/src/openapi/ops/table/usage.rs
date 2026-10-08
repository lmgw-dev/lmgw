//! `ops-usage`: price-sheet mutations (api-docs design §4.7).

use super::{OpArgs, OpDoc, Resp};
use crate::openapi::ops::args;

const TAG: &str = "ops-usage";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "price_set",
        tag: TAG,
        summary: "Set a manual price for one alias or upstream model, in one billable unit",
        description: Some(
            "`unit` says what is counted: `per_mtok` (the default) takes `price_in`, \
             `price_out` and the two cache rates, per 1M tokens; `per_audio_minute`, \
             `per_mchar`, `per_image` and `per_request` take one `price`, per minute of input \
             audio, per 1M input characters, per generated image and per answered request. A \
             scope holds one row per unit, and a request costs the sum of every unit priced \
             for it; a manual row wins over a catalog row of the same scope and unit.",
        ),
        tool: Some("lmgw__price_set"),
        args: OpArgs::Hand(args::price_set),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: Some(
            r#"{"scope_kind":"alias","scope_key":"my-alias","unit":"per_mtok","price_in":3.5,"price_out":10.5}"#,
        ),
    },
    OpDoc {
        name: "price_delete",
        tag: TAG,
        summary: "Delete one price row",
        description: None,
        tool: Some("lmgw__price_delete"),
        args: OpArgs::Tool,
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "prices_sync",
        tag: TAG,
        summary: "Re-sync every enabled upstream's advertised catalog prices",
        description: None,
        tool: Some("lmgw__prices_sync"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<crate::catalog::PriceSyncSummary>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
];
