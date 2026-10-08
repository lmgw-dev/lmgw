//! Field-by-field coverage of `catalog::parse_{openai,anthropic,gemini}_entry`
//! against captured (and a couple of hand-built) catalog samples, per
//! `docs/design/2026-09-17-model-capabilities-design.md` §4.
//!
//! Each fixture under `tests/fixtures/catalogs/` is one raw catalog entry (or,
//! for the Anthropic `models.list` shape, the whole envelope) exactly as
//! captured/hand-written; the parse functions are `pub` so this exercises
//! them directly rather than through a mocked HTTP upstream.
//!
//! The last section covers the price sync's reading of the pricing object
//! (billable-units design §6): which fields become which rows, and that every
//! published field no row takes is counted in `not_synced`. Its rules are
//! tested on `price_updates_for_upstream` directly, and the write path through
//! `sync_prices` against a mocked `/v1/models`.

use std::collections::BTreeMap;

use lmgw_core::catalog::{
    parse_anthropic_entry, parse_gemini_entry, parse_openai_entry, price_updates_for_upstream,
    sync_prices, CatalogReasoning, CatalogStructured, ModelInfo, PriceSyncSummary, PriceUpdate,
    PriceUpdates,
};
use lmgw_core::config::{
    upstream_scope_key, PriceRow, PriceScope, PriceUnit, Protocol, UpstreamKind,
};
use lmgw_core::ops;
use lmgw_core::pricing::{PriceSource, Prices};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store::{self, NewAlias, NewUpstream};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/catalogs/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

// ---------------------------------------------------------------------
// OpenAI-protocol (Kilo/OpenRouter shape)
// ---------------------------------------------------------------------

/// `anthropic/claude-opus-5` — real captured entry with a full
/// `opencode.variants` ladder incl. `none`, and both `structured_outputs` /
/// `response_format` in `supported_parameters`.
#[test]
fn kilo_anthropic_variants() {
    let m = fixture("kilo_anthropic_variants.json");
    let info = parse_openai_entry(&m).expect("parses");

    assert_eq!(info.id, "anthropic/claude-opus-5");
    assert_eq!(info.context_length, Some(1_000_000));
    let pricing = info.pricing.expect("pricing");
    assert_eq!(pricing.prompt, "0.000005");
    assert_eq!(pricing.completion, "0.000025");
    assert_eq!(info.created, Some(1_784_912_544));
    assert_eq!(info.max_output_tokens, Some(128_000));
    assert_eq!(
        info.input_modalities,
        Some(vec![
            "text".to_string(),
            "image".to_string(),
            "file".to_string(),
            "pdf".to_string()
        ])
    );
    assert_eq!(info.output_modalities, Some(vec!["text".to_string()]));
    assert_eq!(info.task.as_deref(), Some("chat"));
    assert_eq!(
        info.reasoning,
        Some(CatalogReasoning {
            kind: "levels".to_string(),
            enabled: None,
            levels: vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
                "max".to_string(),
            ],
            can_disable: Some(true),
        })
    );
    assert_eq!(info.tools, Some(true));
    assert_eq!(
        info.structured_output,
        Some(CatalogStructured {
            json_schema: Some(true),
            json_object: Some(true),
        })
    );
}

/// `stealth/union-alpha` — real captured entry with no `reasoning` /
/// `reasoning_effort` in `supported_parameters` at all: `reasoning` must be
/// absent (`None`), never `fixed`/`false`. Also covers `response_format`
/// present without `structured_outputs` (`json_object` set, `json_schema`
/// absent).
#[test]
fn kilo_no_reasoning() {
    let m = fixture("kilo_no_reasoning.json");
    let info = parse_openai_entry(&m).expect("parses");

    assert_eq!(info.id, "stealth/union-alpha");
    assert_eq!(info.context_length, Some(262_144));
    let pricing = info.pricing.expect("pricing");
    assert_eq!(pricing.prompt, "0");
    assert_eq!(pricing.completion, "0");
    assert_eq!(info.created, Some(1_789_569_723));
    assert_eq!(info.max_output_tokens, Some(131_072));
    assert_eq!(
        info.input_modalities,
        Some(vec!["text".to_string(), "image".to_string()])
    );
    assert_eq!(info.output_modalities, Some(vec!["text".to_string()]));
    assert_eq!(info.task.as_deref(), Some("chat"));
    assert_eq!(
        info.reasoning, None,
        "no reasoning param ⇒ absent, not fixed/false"
    );
    assert_eq!(info.tools, Some(true));
    assert_eq!(
        info.structured_output,
        Some(CatalogStructured {
            json_schema: None,
            json_object: Some(true),
        })
    );
}

/// Synthetic (no embedding model existed in the captured Kilo sample):
/// `architecture.output_modalities == ["embedding"]` ⇒ `task: embedding`,
/// and `supported_parameters` present without `"tools"` ⇒ `tools: Some(false)`
/// (a definite no, not unknown).
#[test]
fn kilo_embedding_task() {
    let m = fixture("kilo_embedding.json");
    let info = parse_openai_entry(&m).expect("parses");

    assert_eq!(info.id, "openai/text-embed-4");
    assert_eq!(info.context_length, Some(8_191));
    assert_eq!(info.max_output_tokens, Some(1));
    assert_eq!(info.input_modalities, Some(vec!["text".to_string()]));
    assert_eq!(info.output_modalities, Some(vec!["embedding".to_string()]));
    assert_eq!(info.task.as_deref(), Some("embedding"));
    assert_eq!(info.reasoning, None);
    assert_eq!(info.tools, Some(false));
    assert_eq!(
        info.structured_output,
        Some(CatalogStructured {
            json_schema: None,
            json_object: None,
        })
    );
}

/// Synthetic: real Kilo `opencode.variants` objects already arrive in
/// canonical order, so this fixture deliberately scrambles the keys (plus a
/// non-canonical `"turbo"` key, which is a preset, not an effort level, and
/// must be dropped) to prove the sort is applied by lmgw, not inherited from
/// the upstream's JSON key order.
#[test]
fn kilo_reordered_levels_sorted_canonically() {
    let m = fixture("kilo_reordered_levels.json");
    let info = parse_openai_entry(&m).expect("parses");

    assert_eq!(info.id, "test/reordered-levels");
    assert_eq!(
        info.reasoning,
        Some(CatalogReasoning {
            kind: "levels".to_string(),
            enabled: None,
            levels: vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
                "max".to_string(),
            ],
            can_disable: Some(true),
        })
    );
}

/// Kilo publishes `variants: {instant, thinking}` for models with a plain
/// on/off switch. Those are presets, not a ladder: the model is a `toggle`
/// with no levels, and nothing says whether it can be disabled.
#[test]
fn kilo_preset_variants_are_a_toggle_not_levels() {
    let mut m = fixture("kilo_reordered_levels.json");
    m["opencode"]["variants"] = serde_json::json!({
        "instant": {"reasoning": {"enabled": false}},
        "thinking": {"reasoning": {"enabled": true}}
    });
    let info = parse_openai_entry(&m).expect("parses");
    assert_eq!(
        info.reasoning,
        Some(CatalogReasoning {
            kind: "toggle".to_string(),
            enabled: None,
            levels: Vec::new(),
            can_disable: None,
        })
    );
}

// ---------------------------------------------------------------------
// Gemini `models.list`
// ---------------------------------------------------------------------

/// `gemini-2.5-pro` — real captured entry, `thinking: true`.
#[test]
fn gemini_thinking_model() {
    let m = fixture("gemini_thinking.json");
    let info = parse_gemini_entry(&m).expect("parses");

    assert_eq!(info.id, "gemini-2.5-pro");
    assert_eq!(info.context_length, Some(1_048_576));
    assert!(info.pricing.is_none(), "Gemini never advertises pricing");
    assert_eq!(info.created, None, "Gemini has no timestamp field");
    assert_eq!(info.max_output_tokens, Some(65_536));
    assert_eq!(
        info.input_modalities, None,
        "Gemini catalog states no modalities"
    );
    assert_eq!(info.output_modalities, None);
    assert_eq!(info.task.as_deref(), Some("chat"));
    assert_eq!(
        info.reasoning,
        Some(CatalogReasoning {
            kind: "toggle".to_string(),
            enabled: None,
            levels: Vec::new(),
            can_disable: None,
        })
    );
    assert_eq!(info.tools, Some(true));
    assert_eq!(info.structured_output, None);
}

/// `gemini-2.5-flash-preview-tts` — real captured entry, no `thinking` key at
/// all (TTS model): `fixed`, `enabled: Some(false)`.
#[test]
fn gemini_tts_no_thinking() {
    let m = fixture("gemini_tts_no_thinking.json");
    let info = parse_gemini_entry(&m).expect("parses");

    assert_eq!(info.id, "gemini-2.5-flash-preview-tts");
    assert_eq!(info.context_length, Some(8_192));
    assert_eq!(info.max_output_tokens, Some(16_384));
    assert_eq!(info.task.as_deref(), Some("chat"));
    assert_eq!(
        info.reasoning,
        Some(CatalogReasoning {
            kind: "fixed".to_string(),
            enabled: Some(false),
            levels: Vec::new(),
            can_disable: None,
        })
    );
    assert_eq!(
        info.tools,
        Some(true),
        "generateContent is in supportedGenerationMethods"
    );
}

/// `gemini-embedding-001` — real captured entry: `embedContent` present ⇒
/// `task: embedding`; no `generateContent` ⇒ `tools: Some(false)`.
#[test]
fn gemini_embedding_model() {
    let m = fixture("gemini_embedding.json");
    let info = parse_gemini_entry(&m).expect("parses");

    assert_eq!(info.id, "gemini-embedding-001");
    assert_eq!(info.context_length, Some(2_048));
    assert_eq!(info.max_output_tokens, Some(1));
    assert_eq!(info.task.as_deref(), Some("embedding"));
    assert_eq!(
        info.reasoning,
        Some(CatalogReasoning {
            kind: "fixed".to_string(),
            enabled: Some(false),
            levels: Vec::new(),
            can_disable: None,
        })
    );
    assert_eq!(info.tools, Some(false));
    assert_eq!(info.structured_output, None);
}

/// `gemini-3.1-flash-live-preview` — real captured entry, `bidiGenerateContent`
/// only (a live-session API lmgw does not route to): neither `embedContent`
/// nor `generateContent` ⇒ `task: chat` (per §4, "neither ⇒ chat"), and
/// `tools: Some(false)`.
#[test]
fn gemini_bidi_only_model() {
    let m = fixture("gemini_bidi_only.json");
    let info = parse_gemini_entry(&m).expect("parses");

    assert_eq!(info.id, "gemini-3.1-flash-live-preview");
    assert_eq!(info.context_length, Some(131_072));
    assert_eq!(info.max_output_tokens, Some(65_536));
    assert_eq!(info.task.as_deref(), Some("chat"));
    assert_eq!(
        info.reasoning,
        Some(CatalogReasoning {
            kind: "fixed".to_string(),
            enabled: Some(false),
            levels: Vec::new(),
            can_disable: None,
        })
    );
    assert_eq!(info.tools, Some(false));
}

// ---------------------------------------------------------------------
// Anthropic `models.list`
// ---------------------------------------------------------------------

fn anthropic_entry(id: &str) -> Value {
    let envelope = fixture("anthropic_models_list.json");
    envelope["data"]
        .as_array()
        .expect("data array")
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} not in fixture"))
        .clone()
}

/// `claude-opus-4-8` — full `capabilities` tree: `image_input.supported`,
/// `thinking.supported`, and every `effort.<level>.supported == true`.
#[test]
fn anthropic_full_capabilities() {
    let m = anthropic_entry("claude-opus-4-8");
    let info = parse_anthropic_entry(&m).expect("parses");

    assert_eq!(info.id, "claude-opus-4-8");
    assert_eq!(info.context_length, Some(1_000_000), "max_input_tokens");
    assert_eq!(info.created, Some(1_769_904_000), "2026-02-01T00:00:00Z");
    assert_eq!(info.max_output_tokens, Some(128_000));
    assert_eq!(
        info.input_modalities,
        Some(vec!["text".to_string(), "image".to_string()])
    );
    assert_eq!(info.output_modalities, Some(vec!["text".to_string()]));
    assert_eq!(info.task.as_deref(), Some("chat"));
    assert_eq!(
        info.reasoning,
        Some(CatalogReasoning {
            kind: "levels".to_string(),
            enabled: None,
            levels: vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
                "max".to_string(),
            ],
            can_disable: None,
        })
    );
    assert_eq!(info.tools, Some(true));
    assert_eq!(
        info.structured_output, None,
        "Anthropic egress drops response_format today"
    );
}

/// `claude-haiku-4-5` — `thinking.supported == true` but `effort.supported ==
/// false` (no per-level breakdown) ⇒ `toggle`, never `levels` with an empty
/// list: `kind` is what a consumer switches on to decide whether to offer an
/// effort choice, and there is none to offer here. The model still thinks, so
/// `fixed` would be wrong too.
#[test]
fn anthropic_thinking_without_effort_levels() {
    let m = anthropic_entry("claude-haiku-4-5");
    let info = parse_anthropic_entry(&m).expect("parses");

    assert_eq!(info.id, "claude-haiku-4-5");
    assert_eq!(info.context_length, Some(200_000));
    assert_eq!(info.created, Some(1_759_276_800), "2025-10-01T00:00:00Z");
    assert_eq!(info.max_output_tokens, Some(64_000));
    assert_eq!(
        info.input_modalities,
        Some(vec!["text".to_string(), "image".to_string()])
    );
    assert_eq!(
        info.reasoning,
        Some(CatalogReasoning {
            kind: "toggle".to_string(),
            enabled: None,
            levels: Vec::new(),
            can_disable: None,
        })
    );
    assert_eq!(info.tools, Some(true));
}

/// `claude-3-haiku-20240307` — no `capabilities` object at all: `reasoning`,
/// `input_modalities`, `output_modalities` all absent (`None`), never
/// defaulted to `fixed`/`false`/`[text]`. `created_at` still parses and
/// `task`/`tools` are unconditional for Anthropic.
#[test]
fn anthropic_no_capabilities_object() {
    let m = anthropic_entry("claude-3-haiku-20240307");
    let info = parse_anthropic_entry(&m).expect("parses");

    assert_eq!(info.id, "claude-3-haiku-20240307");
    assert_eq!(info.context_length, None);
    assert_eq!(info.created, Some(1_709_769_600), "2024-03-07T00:00:00Z");
    assert_eq!(info.max_output_tokens, None);
    assert_eq!(info.input_modalities, None);
    assert_eq!(info.output_modalities, None);
    assert_eq!(info.task.as_deref(), Some("chat"));
    assert_eq!(
        info.reasoning, None,
        "no capabilities object at all ⇒ reasoning absent, not fixed/false"
    );
    assert_eq!(
        info.tools,
        Some(true),
        "tool calling is a base Messages API feature"
    );
    assert_eq!(info.structured_output, None);
}

// ---------------------------------------------------------------------
// Price sync: which pricing fields become which rows (billable-units §6)
// ---------------------------------------------------------------------

fn priced(id: &str, pricing: Value) -> Value {
    json!({ "id": id, "pricing": pricing })
}

/// A model with a usable token price and a `request` fee.
fn with_fee(id: &str, fee: &str) -> Value {
    priced(
        id,
        json!({ "prompt": "0.000001", "completion": "0.000002", "request": fee }),
    )
}

fn updates(entries: &[Value], aliases: &[(&str, &str)]) -> PriceUpdates {
    let models: Vec<ModelInfo> = entries
        .iter()
        .map(|e| parse_openai_entry(e).expect("parses"))
        .collect();
    price_updates_for_upstream(7, &models, aliases)
}

fn of_unit(u: &PriceUpdates, unit: PriceUnit) -> Vec<&PriceUpdate> {
    u.rows.iter().filter(|r| r.unit == unit).collect()
}

fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs.iter().map(|(k, n)| (k.to_string(), *n)).collect()
}

fn on_file(
    scope_kind: PriceScope,
    scope_key: &str,
    unit: PriceUnit,
    source: PriceSource,
) -> PriceRow {
    PriceRow {
        id: 1,
        scope_kind,
        scope_key: scope_key.to_string(),
        unit,
        price_in: None,
        price_out: None,
        price_cache_read: None,
        price_cache_write: None,
        source,
        note: None,
        updated_at: String::new(),
        price: Some(0.01),
    }
}

/// The Kilo fixture publishes `web_search: "0.01"`. No row takes it in v1
/// (Q5), and the count says so instead of dropping it unseen.
#[test]
fn kilo_web_search_is_counted_not_synced() {
    let info = parse_openai_entry(&fixture("kilo_anthropic_variants.json")).expect("parses");
    let pricing = info.pricing.clone().expect("pricing");
    assert_eq!(pricing.web_search.as_deref(), Some("0.01"));
    assert_eq!(pricing.request, None);
    assert!(
        pricing.other.is_empty(),
        "`discount: 0` is a zero, not a price left out: {:?}",
        pricing.other
    );

    let u = price_updates_for_upstream(7, &[info], &[]);
    assert_eq!(u.rows.len(), 1);
    assert_eq!(u.rows[0].unit, PriceUnit::PerMtok);
    assert_eq!(u.not_synced, counts(&[("web_search", 1)]));
}

#[test]
fn a_request_fee_is_synced_beside_a_usable_token_row() {
    let u = updates(&[with_fee("m", "0.005")], &[("m", "fee-a"), ("m", "fee-b")]);
    let tokens = of_unit(&u, PriceUnit::PerMtok);
    let fees = of_unit(&u, PriceUnit::PerRequest);
    assert_eq!(tokens.len(), 2);
    assert_eq!(
        fees.len(),
        2,
        "a fee row for every scope the token row went to"
    );
    for (t, f) in tokens.iter().zip(&fees) {
        assert_eq!((t.scope_kind, &t.scope_key), (f.scope_kind, &f.scope_key));
    }
    for f in fees {
        assert_eq!(f.price, Some(0.005), "USD per request, as published");
        assert_eq!(
            f.prices,
            Prices {
                source: PriceSource::Catalog,
                ..Default::default()
            },
            "no token rate on a non-token row"
        );
        assert!(!f.update_only);
    }
    assert!(u.not_synced.is_empty(), "{:?}", u.not_synced);

    // A bare JSON number reads the same as the documented string.
    let u = updates(
        &[priced(
            "m",
            json!({ "prompt": 0.000001, "completion": 0.000002, "request": 0.005 }),
        )],
        &[],
    );
    assert_eq!(of_unit(&u, PriceUnit::PerRequest)[0].price, Some(0.005));
}

/// A fee without its token row would turn an unpriced model, which is on the
/// worklist, into one priced at the fee alone: wrong downward and silent.
#[test]
fn a_request_fee_without_a_usable_token_price_writes_nothing() {
    for pricing in [
        json!({ "prompt": "n/a", "completion": "0.000002", "request": "0.005" }),
        json!({ "prompt": "-1", "completion": "-1", "request": "0.005" }),
        json!({ "completion": "0.000002", "request": "0.005" }),
    ] {
        let u = updates(&[priced("m", pricing.clone())], &[]);
        assert!(u.rows.is_empty(), "{pricing}: {:?}", u.rows);
        assert_eq!(u.unpriced_models, 1, "{pricing}");
        assert!(
            u.not_synced.is_empty(),
            "an unpriced model is counted once, as unpriced: {pricing}"
        );
    }
}

#[test]
fn a_request_fee_that_is_not_a_price_is_counted_not_written() {
    for fee in ["-1", "NaN", "inf", "about a cent"] {
        let u = updates(&[with_fee("m", fee)], &[]);
        assert!(of_unit(&u, PriceUnit::PerRequest).is_empty(), "{fee}");
        assert_eq!(
            of_unit(&u, PriceUnit::PerMtok).len(),
            1,
            "{fee}: the token row still goes on file"
        );
        assert_eq!(u.not_synced, counts(&[("request", 1)]), "{fee}");
    }
}

#[test]
fn a_zero_request_fee_only_updates_a_catalog_row_on_file() {
    let u = updates(&[with_fee("m", "0")], &[("m", "fee-a")]);
    let fee = of_unit(&u, PriceUnit::PerRequest);
    assert_eq!(fee.len(), 1);
    assert_eq!(fee[0].price, Some(0.0));
    assert!(fee[0].update_only);
    assert!(
        u.not_synced.is_empty(),
        "a published zero leaves nothing out"
    );

    let units = |rows: &[PriceRow]| u.to_write(rows).map(|r| r.unit).collect::<Vec<_>>();
    assert_eq!(
        units(&[]),
        [PriceUnit::PerMtok],
        "no fee on file: no zero row"
    );
    assert_eq!(
        units(&[on_file(
            PriceScope::Alias,
            "fee-a",
            PriceUnit::PerRequest,
            PriceSource::Catalog
        )]),
        [PriceUnit::PerMtok, PriceUnit::PerRequest],
        "the fee the provider dropped is set to 0"
    );
    // Nothing else counts as the row to update: the owner's own fee, another
    // scope's, or another unit's.
    for other in [
        on_file(
            PriceScope::Alias,
            "fee-a",
            PriceUnit::PerRequest,
            PriceSource::Manual,
        ),
        on_file(
            PriceScope::Alias,
            "fee-b",
            PriceUnit::PerRequest,
            PriceSource::Catalog,
        ),
        on_file(
            PriceScope::UpstreamModel,
            "7:m",
            PriceUnit::PerRequest,
            PriceSource::Catalog,
        ),
        on_file(
            PriceScope::Alias,
            "fee-a",
            PriceUnit::PerImage,
            PriceSource::Catalog,
        ),
    ] {
        assert_eq!(
            units(std::slice::from_ref(&other)),
            [PriceUnit::PerMtok],
            "{other:?}"
        );
    }
}

/// Every field no row takes is named, the documented ones and any a provider
/// adds; a field published as zero is not.
#[test]
fn every_other_published_price_is_named_and_counted() {
    let entry = priced(
        "m",
        json!({
            "prompt": "0.000001",
            "completion": "0.000002",
            "image": "0.0012",
            "web_search": "0.01",
            "internal_reasoning": "0.000002",
            "image_output": "0.00003",
            "input_cache_write_1h": "0.000002",
            "overrides": [{ "when": "long context" }],
            "discount": 0.1,
            // Zeros and empties cost nothing, so leaving them out loses nothing.
            "audio": "0",
            "input_audio_cache": 0,
            "image_token": "",
            "audio_output": null,
            // A named field in a shape no price comes in is not "absent".
            "request": { "usd": 0.01 },
        }),
    );
    let info = parse_openai_entry(&entry).expect("parses");
    let pricing = info.pricing.clone().expect("pricing");
    assert_eq!(pricing.request, None);
    assert_eq!(pricing.image.as_deref(), Some("0.0012"));
    assert_eq!(
        pricing.other,
        [
            "discount",
            "image_output",
            "input_cache_write_1h",
            "internal_reasoning",
            "overrides",
            "request"
        ]
    );

    let u = price_updates_for_upstream(7, &[info], &[]);
    assert_eq!(u.rows.len(), 1, "the token row only");
    assert_eq!(
        u.not_synced,
        counts(&[
            ("discount", 1),
            ("image", 1),
            ("image_output", 1),
            ("input_cache_write_1h", 1),
            ("internal_reasoning", 1),
            ("overrides", 1),
            ("request", 1),
            ("web_search", 1),
        ])
    );
}

#[test]
fn published_zeros_are_not_counted() {
    let u = updates(
        &[priced(
            "m",
            json!({
                "prompt": "0.000001", "completion": "0.000002", "request": "0",
                "image": "0", "web_search": 0, "internal_reasoning": "0.0",
                "input_cache_read": "0", "discount": 0, "overrides": [],
            }),
        )],
        &[],
    );
    assert!(u.not_synced.is_empty(), "{:?}", u.not_synced);
}

/// A cache rate that did not become one bills at the input rate. That errs
/// upward, but it is still a published number lmgw did not apply.
#[test]
fn a_published_cache_rate_that_is_not_a_price_is_counted() {
    let u = updates(
        &[priced(
            "m",
            json!({
                "prompt": "0.000005", "completion": "0.000025",
                "input_cache_read": "cheap", "input_cache_write": "0.00000625",
            }),
        )],
        &[],
    );
    assert_eq!(u.rows[0].prices.price_cache_read, None);
    assert_eq!(u.rows[0].prices.price_cache_write, Some(6.25));
    assert_eq!(u.not_synced, counts(&[("input_cache_read", 1)]));
}

#[test]
fn not_synced_counts_models_not_rows() {
    let search = |id: &str| {
        priced(
            id,
            json!({ "prompt": "0.000001", "completion": "0.000002", "web_search": "0.01" }),
        )
    };
    let u = updates(&[search("a"), search("b")], &[("a", "a-1"), ("a", "a-2")]);
    assert_eq!(u.rows.len(), 3);
    assert_eq!(u.not_synced, counts(&[("web_search", 2)]));
}

/// A priced model that publishes no `request` at all marks its scopes, and a
/// catalog fee on file there is stale. Anything else about the fee field —
/// a fee, a zero, a value that is not a price — is not "no fee".
#[test]
fn a_model_that_publishes_no_fee_marks_its_catalog_fee_stale() {
    let tokens = json!({ "prompt": "0.000001", "completion": "0.000002" });
    let u = updates(&[priced("m", tokens.clone())], &[("m", "fee-a")]);
    assert_eq!(u.no_fee, [(PriceScope::Alias, "fee-a".to_string())]);

    let catalog_fee = on_file(
        PriceScope::Alias,
        "fee-a",
        PriceUnit::PerRequest,
        PriceSource::Catalog,
    );
    let kept = [
        on_file(
            PriceScope::Alias,
            "fee-a",
            PriceUnit::PerRequest,
            PriceSource::Manual,
        ),
        on_file(
            PriceScope::Alias,
            "fee-a",
            PriceUnit::PerMtok,
            PriceSource::Catalog,
        ),
        on_file(
            PriceScope::Alias,
            "fee-b",
            PriceUnit::PerRequest,
            PriceSource::Catalog,
        ),
    ];
    let mut rows = kept.to_vec();
    rows.push(catalog_fee.clone());
    let removed: Vec<&PriceRow> = u.to_remove(&rows).collect();
    assert_eq!(
        removed,
        [&catalog_fee],
        "only the catalog fee of that scope"
    );

    for request in [
        json!("0.01"),
        json!("0"),
        json!("-1"),
        json!({ "usd": 0.01 }),
    ] {
        let mut p = tokens.clone();
        p["request"] = request.clone();
        let u = updates(&[priced("m", p)], &[("m", "fee-a")]);
        assert!(u.no_fee.is_empty(), "{request}");
        assert_eq!(u.to_remove(&rows).count(), 0, "{request}");
    }
    let mut p = tokens.clone();
    p["request"] = json!("");
    assert_eq!(
        updates(&[priced("m", p)], &[]).no_fee.len(),
        1,
        "empty is absent"
    );
    // An unpriced model syncs nothing, removals included.
    let u = updates(
        &[priced(
            "m",
            json!({ "prompt": "n/a", "completion": "0.000002" }),
        )],
        &[],
    );
    assert!(u.no_fee.is_empty());
}

// --- through `sync_prices`, against a mocked `/v1/models` ---------------

async fn serve_models(server: &MockServer, models: Value) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": models })))
        .mount(server)
        .await;
}

/// A gateway with one cloud upstream whose catalog lists `models`.
async fn catalog_gateway(models: Value) -> (SharedState, MockServer, i64) {
    let st = AppState::init_for_tests().await.unwrap();
    let server = MockServer::start().await;
    serve_models(&server, models).await;
    let up = store::insert_upstream(
        &st.db,
        &NewUpstream {
            name: "kilo".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", server.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 10_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    st.reload_snapshot().await.unwrap();
    (st, server, up)
}

async fn add_alias(st: &SharedState, up: i64, model: &str, alias: &str) {
    store::insert_alias(
        &st.db,
        &NewAlias {
            alias: alias.into(),
            upstream_id: up,
            upstream_model_id: model.into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    st.reload_snapshot().await.unwrap();
}

/// One sync, past the catalog cache.
async fn sync(st: &SharedState, up: i64) -> PriceSyncSummary {
    st.catalog.invalidate(up).await;
    sync_prices(st).await.expect("sync")
}

async fn rows_in(st: &SharedState, unit: PriceUnit) -> Vec<PriceRow> {
    store::list_prices(&st.db)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.unit == unit)
        .collect()
}

/// What `lmgw__prices_sync` and the Prices card read: the op's JSON, which
/// carries `not_synced` at the top and per upstream.
#[tokio::test]
async fn the_sync_reports_the_kilo_web_search_as_not_synced() {
    let (st, _server, up) = catalog_gateway(json!([fixture("kilo_anthropic_variants.json")])).await;
    let v = ops::prices_sync(&st).await.expect("sync");

    assert_eq!(v["rows_written"], 1, "{v}");
    assert_eq!(v["not_synced"], json!({ "web_search": 1 }), "{v}");
    let mine = v["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["upstream_id"] == up)
        .expect("the upstream's own line");
    assert_eq!(mine["not_synced"], json!({ "web_search": 1 }), "{v}");
    let rows = store::list_prices(&st.db).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].unit, PriceUnit::PerMtok);
}

#[tokio::test]
async fn the_sync_writes_the_request_fee_beside_the_token_row() {
    let (st, _server, up) = catalog_gateway(json!([with_fee("vendor/m", "0.002")])).await;
    add_alias(&st, up, "vendor/m", "fee-chat").await;
    let s = sync(&st, up).await;
    assert_eq!(s.rows_written, 2);
    assert!(s.not_synced.is_empty(), "{:?}", s.not_synced);

    let fee = rows_in(&st, PriceUnit::PerRequest).await;
    assert_eq!(fee.len(), 1, "{fee:?}");
    assert_eq!(
        (fee[0].scope_kind, fee[0].scope_key.as_str(), fee[0].source),
        (PriceScope::Alias, "fee-chat", PriceSource::Catalog)
    );
    assert_eq!(fee[0].price, Some(0.002));
    assert_eq!((fee[0].price_in, fee[0].price_out), (None, None));

    // The very next request is priced with both: the sync reloads the snapshot.
    let sheet = st
        .snapshot()
        .sheet_for("fee-chat", Some(up), Some("vendor/m"));
    assert_eq!(
        sheet.request.map(|r| (r.price, r.source)),
        Some((0.002, PriceSource::Catalog))
    );
    assert_eq!(sheet.tokens.and_then(|t| t.price_in), Some(1.0));
}

#[tokio::test]
async fn the_sync_writes_no_fee_for_a_model_whose_prompt_does_not_parse() {
    let (st, _server, up) = catalog_gateway(json!([priced(
        "m",
        json!({ "prompt": "n/a", "completion": "0.000002", "request": "0.002" }),
    )]))
    .await;
    let s = sync(&st, up).await;
    assert_eq!((s.rows_written, s.unpriced_models), (0, 1));
    assert!(store::list_prices(&st.db).await.unwrap().is_empty());
    assert!(st
        .snapshot()
        .sheet_for("x", Some(up), Some("m"))
        .request
        .is_none());
}

#[tokio::test]
async fn a_zero_fee_updates_the_catalog_row_on_file_and_creates_none() {
    let (st, server, up) = catalog_gateway(json!([with_fee("a", "0.01")])).await;
    sync(&st, up).await;
    let before = rows_in(&st, PriceUnit::PerRequest).await;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].price, Some(0.01));

    // The owner's own fee on `b` is not the catalog's to touch.
    let b = upstream_scope_key(up, "b");
    store::upsert_price(
        &st.db,
        PriceScope::UpstreamModel,
        &b,
        PriceUnit::PerRequest,
        &Prices {
            source: PriceSource::Manual,
            ..Default::default()
        },
        Some(0.5),
        None,
    )
    .await
    .unwrap();
    st.reload_snapshot().await.unwrap();

    serve_models(&server, json!([with_fee("a", "0"), with_fee("b", "0")])).await;
    let s = sync(&st, up).await;
    assert_eq!(s.rows_written, 3, "two token rows and a's fee");
    assert!(s.not_synced.is_empty(), "{:?}", s.not_synced);

    let after = rows_in(&st, PriceUnit::PerRequest).await;
    let a = after
        .iter()
        .find(|r| r.scope_key == upstream_scope_key(up, "a"))
        .expect("a's fee row");
    assert_eq!(
        (a.id, a.source, a.price),
        (before[0].id, PriceSource::Catalog, Some(0.0))
    );
    let on_b: Vec<_> = after.iter().filter(|r| r.scope_key == b).collect();
    assert_eq!(on_b.len(), 1, "no catalog zero row created: {on_b:?}");
    assert_eq!(
        (on_b[0].source, on_b[0].price),
        (PriceSource::Manual, Some(0.5))
    );
}

/// Catalog rows mirror the catalog: a fee the provider stopped publishing
/// stops charging, and the owner's own fee on the same scope stays.
#[tokio::test]
async fn a_fee_the_catalog_stopped_publishing_is_removed_and_the_manual_row_stays() {
    let (st, server, up) = catalog_gateway(json!([with_fee("a", "0.01")])).await;
    sync(&st, up).await;
    let a = upstream_scope_key(up, "a");
    store::upsert_price(
        &st.db,
        PriceScope::UpstreamModel,
        &a,
        PriceUnit::PerRequest,
        &Prices {
            source: PriceSource::Manual,
            ..Default::default()
        },
        Some(0.5),
        None,
    )
    .await
    .unwrap();
    st.reload_snapshot().await.unwrap();
    assert_eq!(rows_in(&st, PriceUnit::PerRequest).await.len(), 2);

    serve_models(
        &server,
        json!([priced(
            "a",
            json!({ "prompt": "0.000001", "completion": "0.000002" })
        )]),
    )
    .await;
    let s = sync(&st, up).await;
    assert_eq!((s.rows_written, s.rows_removed), (1, 1), "{s:?}");
    let mine = s.upstreams.iter().find(|u| u.upstream_id == up).unwrap();
    assert_eq!(mine.rows_removed, 1);

    let fees = rows_in(&st, PriceUnit::PerRequest).await;
    assert_eq!(fees.len(), 1, "{fees:?}");
    assert_eq!(
        (fees[0].scope_key.as_str(), fees[0].source, fees[0].price),
        (a.as_str(), PriceSource::Manual, Some(0.5))
    );
    assert_eq!(
        rows_in(&st, PriceUnit::PerMtok).await.len(),
        1,
        "the token row stays"
    );
    // The sync reloaded the snapshot: the manual fee is what prices `a` now.
    let sheet = st.snapshot().sheet_for("x", Some(up), Some("a"));
    assert_eq!(
        sheet.request.map(|r| (r.price, r.source)),
        Some((0.5, PriceSource::Manual))
    );
}
