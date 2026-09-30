//! Price-sheet mutations: `lmgw__prices_sync`, `lmgw__price_set`,
//! `lmgw__price_delete`.

use crate::mcp::selfadmin::{enum_p, int_p, num_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__prices_sync",
            writes: true,
            description:
                "Re-read every enabled upstream's model catalog and upsert its advertised \
                 prices (source=catalog) into the price table, per 1M tokens. A model that \
                 advertises no price gets no row — never a zero, which would read as an \
                 authoritative price and be wrong downward. Never touches a manual row \
                 (lmgw__price_set), which always wins over a catalog row for the same scope. \
                 Takes effect immediately, no restart. Reports rows written and models with \
                 no advertised price, per upstream.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__price_set",
            writes: true,
            description: "Set a manual price sheet for one scope: an alias, or an upstream_model \
                 ('<upstream_id>:<upstream_model_id>', from lmgw__prices — what an \
                 expose_all passthrough request resolves to). A manual row always wins over \
                 a catalog-synced row for the same scope and is never overwritten by \
                 lmgw__prices_sync — this is how you price an upstream that publishes no \
                 catalog pricing at all (Gemini, for one). Prices are per 1M tokens in the \
                 gateway's configured currency (lmgw__settings). Omitting \
                 price_cache_read/price_cache_write bills those tokens at price_in instead \
                 of inventing a discount.",
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
                ("price_in", num_p("Input price per 1M tokens.")),
                ("price_out", num_p("Output price per 1M tokens.")),
                (
                    "price_cache_read",
                    num_p("Cache-read price per 1M tokens. Omit to bill at price_in."),
                ),
                (
                    "price_cache_write",
                    num_p("Cache-write price per 1M tokens. Omit to bill at price_in."),
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
