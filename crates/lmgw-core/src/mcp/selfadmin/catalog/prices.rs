//! Price-sheet mutations: `lmgw__prices_sync`, `lmgw__price_set`,
//! `lmgw__price_delete`.

use crate::config::PriceUnit;
use crate::mcp::selfadmin::{enum_p, int_p, num_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    // The unit enum and what `price` is per come from the one table the
    // gateway prices with (billable-units design §2.1), so the tool cannot
    // offer a unit the op refuses or describe a scale it does not use.
    let units: Vec<&str> = PriceUnit::ALL.iter().map(|u| u.as_str()).collect();
    let price_scales = PriceUnit::ALL
        .iter()
        .filter(|u| !u.is_tokens())
        .map(|u| format!("{u} {}", u.scale()))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        Builtin {
            name: "lmgw__prices_sync",
            writes: true,
            description:
                "Re-read every enabled upstream's model catalog and upsert its advertised \
                 prices (source=catalog) into the price table: the token rates (unit \
                 per_mtok, per 1M tokens, cache read and write included) and, beside a \
                 usable token row, a per-request fee (unit per_request). A model that \
                 advertises no price gets no row — never a zero, which would read as an \
                 authoritative price and be wrong downward. Never touches a manual row \
                 (lmgw__price_set), which always wins over a catalog row for the same scope \
                 and unit. A catalog per_request row whose fee the catalog no longer \
                 publishes is removed. Takes effect immediately, no restart. Reports, per \
                 upstream and in total: rows written, rows_removed (those fee rows), models \
                 with no advertised price, and not_synced — each advertised price field lmgw \
                 has no unit for (image, web_search, …) with how many models publish it, \
                 counted rather than silently dropped.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__price_set",
            writes: true,
            description: "Set a manual price for one scope in one billable unit: an alias, or an \
                 upstream_model ('<upstream_id>:<upstream_model_id>', from lmgw__prices — \
                 what an expose_all passthrough request resolves to). unit says what is \
                 counted: per_mtok (the default) takes price_in/price_out and the two cache \
                 rates, per 1M tokens; every other unit takes one price — per_audio_minute \
                 per minute of input audio, per_mchar per 1M characters of input text, \
                 per_image per generated image, per_request per answered request. A scope \
                 may hold one row per unit, and a request costs the sum of every unit priced \
                 for it (tokens plus a request fee, say). A manual row always wins over a \
                 catalog-synced row for the same scope and unit and is never overwritten by \
                 lmgw__prices_sync — this is how you price an upstream that publishes no \
                 catalog pricing at all (Gemini, for one). Prices are in the gateway's \
                 configured currency (lmgw__settings). Omitting \
                 price_cache_read/price_cache_write bills those tokens at price_in instead \
                 of inventing a discount. A manual 0 for a unit drops a catalog rate of it.",
            props: vec![
                (
                    "scope_kind",
                    enum_p("What this price applies to.", &["alias", "upstream_model"]),
                ),
                (
                    "scope_key",
                    str_p(
                        "The alias name for scope_kind=alias, or \
                         '<upstream_id>:<upstream_model_id>' for scope_kind=upstream_model \
                         (lmgw__prices lists existing scope_keys).",
                    ),
                ),
                (
                    "unit",
                    enum_p(
                        "What the price counts. Default per_mtok (tokens). A row's unit is \
                         part of its key: setting another unit adds a row beside the existing \
                         one rather than changing it.",
                        &units,
                    ),
                ),
                (
                    "price_in",
                    num_p("per_mtok only: input price per 1M tokens."),
                ),
                (
                    "price_out",
                    num_p("per_mtok only: output price per 1M tokens."),
                ),
                (
                    "price_cache_read",
                    num_p(
                        "per_mtok only: cache-read price per 1M tokens. Omit to bill at \
                         price_in.",
                    ),
                ),
                (
                    "price_cache_write",
                    num_p(
                        "per_mtok only: cache-write price per 1M tokens. Omit to bill at \
                         price_in.",
                    ),
                ),
                (
                    "price",
                    num_p(&format!(
                        "Every unit but per_mtok: its one rate — {price_scales}."
                    )),
                ),
                (
                    "note",
                    str_p("Free-text note, e.g. where this number came from."),
                ),
            ],
            required: &["scope_kind", "scope_key"],
        },
        Builtin {
            name: "lmgw__price_delete",
            writes: true,
            description:
                "Delete one price row by id (from lmgw__prices). Deleting a catalog row is \
                 harmless — the next lmgw__prices_sync recreates it if the upstream still \
                 advertises a price. Deleting the only manual row for a scope lets that \
                 scope's catalog row, if any, show through again.",
            props: vec![("id", int_p("Price row id, from lmgw__prices."))],
            required: &["id"],
        },
    ]
}
