//! Field-by-field coverage of `catalog::parse_{openai,anthropic,gemini}_entry`
//! against captured (and a couple of hand-built) catalog samples, per
//! `docs/design/2026-09-17-model-capabilities-design.md` §4.
//!
//! Each fixture under `tests/fixtures/catalogs/` is one raw catalog entry (or,
//! for the Anthropic `models.list` shape, the whole envelope) exactly as
//! captured/hand-written; the parse functions are `pub` so this exercises
//! them directly rather than through a mocked HTTP upstream.

use lmgw_core::catalog::{
    parse_anthropic_entry, parse_gemini_entry, parse_openai_entry, CatalogReasoning,
    CatalogStructured,
};
use serde_json::Value;

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
