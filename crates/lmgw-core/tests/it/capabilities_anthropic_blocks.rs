//! Anthropic's capability blocks on the Anthropic-shaped model entry
//! (`capabilities::anthropic_blocks`, model-capabilities design §2.2.1):
//! each block's true and false cases, from hand-built lmgw facts.

use lmgw_core::capabilities::anthropic_blocks::{
    anthropic_blocks, anthropic_capabilities, AnthropicSupport,
};
use lmgw_core::capabilities::{
    ContextEditingCaps, ModelCapabilities, ReasoningCaps, StructuredOutputCaps,
    CHAT_ENDPOINTS_OPENAI,
};
use serde_json::{json, Value};

const YES: AnthropicSupport = AnthropicSupport { supported: true };
const NO: AnthropicSupport = AnthropicSupport { supported: false };

/// A chat model served on `/v1/messages` that states nothing else.
fn chat() -> ModelCapabilities {
    ModelCapabilities {
        task: "chat".into(),
        endpoints: CHAT_ENDPOINTS_OPENAI
            .iter()
            .map(|s| s.to_string())
            .collect(),
        source: "config".into(),
        ..Default::default()
    }
}

fn with_reasoning(r: ReasoningCaps) -> ModelCapabilities {
    ModelCapabilities {
        reasoning: Some(r),
        ..chat()
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// A local row whose template reads `enable_thinking` and the effort
/// variable, with preserved reasoning — every thinking type lmgw serves.
fn local_levels() -> ReasoningCaps {
    ReasoningCaps {
        kind: "levels".into(),
        enabled: Some(true),
        levels: strings(&["low", "medium", "high"]),
        can_disable: Some(true),
        preserve_history: Some(true),
        control: strings(&[
            "x-lmgw-reasoning",
            "x-lmgw-reasoning-effort",
            "reasoning_effort",
            "chat_template_kwargs.enable_thinking",
        ]),
        ..Default::default()
    }
}

#[test]
fn blocks_lmgw_does_not_serve_yet_are_false_on_every_model() {
    for caps in [
        chat(),
        with_reasoning(local_levels()),
        ModelCapabilities {
            vision: Some(true),
            structured_output: Some(StructuredOutputCaps {
                json_schema: Some(true),
                json_object: Some(true),
            }),
            ..chat()
        },
    ] {
        let b = anthropic_blocks(&caps);
        assert_eq!(b.batch, NO);
        assert_eq!(b.citations, NO);
        assert_eq!(b.code_execution, NO);
        // No `context_editing`: not an Anthropic route.
        assert!(!b.context_management.supported);
        assert_eq!(b.context_management.clear_tool_uses_20250919, NO);
        assert_eq!(b.pdf_input, NO);
        assert!(!b.server_tools.supported);
        assert_eq!(b.server_tools.code_execution, NO);
        assert_eq!(b.server_tools.web_search, NO);
    }
}

/// `pdf_input` is the catalog's `pdf` modality, and only on an Anthropic
/// route (`context_editing` present), the one route sent `document` blocks.
#[test]
fn pdf_input_follows_the_catalog_only_on_an_anthropic_route() {
    let pdf = Some(strings(&["text", "image", "pdf"]));
    let anthropic = ModelCapabilities {
        input_modalities: pdf.clone(),
        context_editing: Some(ContextEditingCaps::default()),
        ..chat()
    };
    assert_eq!(anthropic_blocks(&anthropic).pdf_input, YES);
    let other_route = ModelCapabilities {
        input_modalities: pdf,
        ..chat()
    };
    assert_eq!(anthropic_blocks(&other_route).pdf_input, NO);
    let no_pdf = ModelCapabilities {
        input_modalities: Some(strings(&["text", "image"])),
        ..anthropic.clone()
    };
    assert_eq!(anthropic_blocks(&no_pdf).pdf_input, NO);
}

#[test]
fn image_input_follows_vision_only_on_the_messages_route() {
    let seeing = ModelCapabilities {
        vision: Some(true),
        ..chat()
    };
    assert_eq!(anthropic_blocks(&seeing).image_input, YES);
    for vision in [Some(false), None] {
        let caps = ModelCapabilities { vision, ..chat() };
        assert_eq!(anthropic_blocks(&caps).image_input, NO, "{vision:?}");
    }
    // A vision embedder is not served on /v1/messages at all.
    let embedder = ModelCapabilities {
        task: "embedding".into(),
        endpoints: strings(&["/v1/embeddings"]),
        vision: Some(true),
        ..chat()
    };
    assert_eq!(anthropic_blocks(&embedder).image_input, NO);
}

#[test]
fn structured_outputs_follows_json_schema() {
    let so = |json_schema| ModelCapabilities {
        structured_output: Some(StructuredOutputCaps {
            json_schema,
            json_object: Some(true),
        }),
        ..chat()
    };
    assert_eq!(anthropic_blocks(&so(Some(true))).structured_outputs, YES);
    assert_eq!(anthropic_blocks(&so(Some(false))).structured_outputs, NO);
    assert_eq!(anthropic_blocks(&so(None)).structured_outputs, NO);
    assert_eq!(anthropic_blocks(&chat()).structured_outputs, NO);
}

#[test]
fn effort_is_supported_with_levels_and_names_each_level() {
    let b = anthropic_blocks(&with_reasoning(ReasoningCaps {
        levels: strings(&["minimal", "low", "medium", "high", "xhigh", "max"]),
        ..local_levels()
    }));
    assert!(b.effort.supported);
    for l in [
        b.effort.low,
        b.effort.medium,
        b.effort.high,
        b.effort.xhigh,
        b.effort.max,
    ] {
        assert_eq!(l, YES);
    }

    let b = anthropic_blocks(&with_reasoning(local_levels()));
    assert!(b.effort.supported);
    assert_eq!(
        [b.effort.low, b.effort.medium, b.effort.high],
        [YES, YES, YES]
    );
    assert_eq!(b.effort.xhigh, NO);
    assert_eq!(b.effort.max, NO);

    // A toggle has no levels to choose, whatever a stray list says.
    let b = anthropic_blocks(&with_reasoning(ReasoningCaps {
        kind: "toggle".into(),
        ..local_levels()
    }));
    assert!(!b.effort.supported);
    assert_eq!(b.effort.low, NO);

    let b = anthropic_blocks(&chat());
    assert!(!b.effort.supported);
    assert_eq!(b.effort.high, NO);
}

#[test]
fn a_local_row_with_every_control_serves_the_types_it_reaches() {
    let b = anthropic_blocks(&with_reasoning(local_levels()));
    assert!(b.thinking.supported);
    assert_eq!(b.thinking.types.adaptive, YES);
    assert_eq!(b.thinking.types.disabled, YES);
    // Only an Anthropic route sends `between_tools` as such; a local row
    // gets its template's off, preserved history or not.
    assert_eq!(b.thinking.types.between_tools, NO);
    // llama-server ignores a per-request budget: not published in control.
    assert_eq!(b.thinking.types.enabled, NO);
}

#[test]
fn thinking_is_unsupported_without_reasoning_or_when_fixed_off() {
    for caps in [
        chat(),
        with_reasoning(ReasoningCaps {
            kind: "fixed".into(),
            enabled: Some(false),
            ..Default::default()
        }),
        // Fixed with an unknown state: lmgw does not know it thinks.
        with_reasoning(ReasoningCaps {
            kind: "fixed".into(),
            ..Default::default()
        }),
    ] {
        let b = anthropic_blocks(&caps);
        assert!(!b.thinking.supported, "{caps:?}");
        let t = b.thinking.types;
        assert_eq!(
            [t.adaptive, t.between_tools, t.disabled, t.enabled],
            [NO; 4],
            "{caps:?}"
        );
    }
}

#[test]
fn a_model_that_always_thinks_supports_adaptive_and_nothing_it_cannot_change() {
    let b = anthropic_blocks(&with_reasoning(ReasoningCaps {
        kind: "fixed".into(),
        enabled: Some(true),
        ..Default::default()
    }));
    assert!(b.thinking.supported);
    assert_eq!(b.thinking.types.adaptive, YES);
    assert_eq!(b.thinking.types.disabled, NO);
    assert_eq!(b.thinking.types.enabled, NO);
    assert_eq!(b.thinking.types.between_tools, NO);
}

/// `for_catalog`'s Anthropic row: levels from the effort tree, the native
/// fields in control, and what the catalog says about the thinking types.
fn anthropic_row(types: &[&str], can_disable: Option<bool>) -> ModelCapabilities {
    with_reasoning(ReasoningCaps {
        kind: "levels".into(),
        levels: strings(&["low", "medium", "high", "max"]),
        can_disable,
        control: strings(&[
            "x-lmgw-reasoning",
            "x-lmgw-reasoning-effort",
            "x-lmgw-reasoning-budget",
            "thinking",
            "output_config.effort",
        ]),
        thinking_types: strings(types),
        ..Default::default()
    })
}

#[test]
fn an_anthropic_route_maps_the_models_published_thinking_types() {
    // Takes adaptive and between_tools, cannot switch off (a Sonnet 5.5
    // shape): the route sends between_tools as written, so it is served.
    let b = anthropic_blocks(&anthropic_row(&["adaptive", "between_tools"], Some(false)));
    assert!(b.thinking.supported);
    let t = b.thinking.types;
    assert_eq!(
        [t.adaptive, t.between_tools, t.disabled, t.enabled],
        [YES, YES, NO, NO]
    );
    assert!(b.effort.supported);
    assert_eq!(b.effort.max, YES);
    assert_eq!(b.effort.xhigh, NO);

    // Can switch off; between_tools not among its types (an Opus 5 or
    // Haiku 5.5 shape).
    let t = anthropic_blocks(&anthropic_row(&["adaptive", "disabled"], Some(true)))
        .thinking
        .types;
    assert_eq!(
        [t.adaptive, t.between_tools, t.disabled, t.enabled],
        [YES, NO, YES, NO]
    );

    // The budget form only, no adaptive: on and effort are not served,
    // the budget is.
    let t = anthropic_blocks(&anthropic_row(&["enabled"], None))
        .thinking
        .types;
    assert_eq!([t.adaptive, t.disabled, t.enabled], [NO, NO, YES]);

    // Nothing published: Anthropic's schema has no unknown, so false.
    let t = anthropic_blocks(&anthropic_row(&[], None)).thinking.types;
    assert_eq!(
        [t.adaptive, t.between_tools, t.disabled, t.enabled],
        [NO; 4]
    );
}

#[test]
fn a_gemini_route_takes_the_budget_form_but_not_a_bare_on() {
    let b = anthropic_blocks(&with_reasoning(ReasoningCaps {
        kind: "toggle".into(),
        control: strings(&[
            "x-lmgw-reasoning",
            "x-lmgw-reasoning-effort",
            "x-lmgw-reasoning-budget",
        ]),
        ..Default::default()
    }));
    assert!(b.thinking.supported);
    assert_eq!(b.thinking.types.enabled, YES);
    assert_eq!(b.thinking.types.adaptive, NO);
}

#[test]
fn an_openai_route_takes_adaptive_only_when_the_model_thinks_by_default() {
    let openai = |enabled, can_disable| {
        with_reasoning(ReasoningCaps {
            kind: "levels".into(),
            enabled,
            levels: strings(&["low", "medium", "high"]),
            can_disable,
            control: strings(&[
                "x-lmgw-reasoning",
                "x-lmgw-reasoning-effort",
                "reasoning_effort",
            ]),
            ..Default::default()
        })
    };
    let b = anthropic_blocks(&openai(None, Some(true)));
    assert!(b.thinking.supported);
    assert_eq!(b.thinking.types.adaptive, NO);
    assert_eq!(b.thinking.types.enabled, NO);
    assert_eq!(b.thinking.types.disabled, YES);

    let b = anthropic_blocks(&openai(Some(true), None));
    assert_eq!(b.thinking.types.adaptive, YES);
    assert_eq!(b.thinking.types.disabled, NO);
}

fn with_editing(supported: Option<bool>, edits: &[&str]) -> ModelCapabilities {
    with_compaction(supported, edits, &[])
}

fn with_compaction(supported: Option<bool>, edits: &[&str], types: &[&str]) -> ModelCapabilities {
    ModelCapabilities {
        context_editing: Some(ContextEditingCaps {
            supported,
            edits: strings(edits),
            compaction: strings(types),
        }),
        ..chat()
    }
}

#[test]
fn context_management_follows_the_anthropic_routes_published_edits() {
    // Every edit the catalog names, the compaction edit included: its
    // `compaction` block comes back through lmgw.
    let b = anthropic_blocks(&with_editing(
        Some(true),
        &[
            "clear_thinking_20251015",
            "clear_tool_uses_20250919",
            "compact_20260112",
        ],
    ));
    assert!(b.context_management.supported);
    assert_eq!(b.context_management.clear_thinking_20251015, YES);
    assert_eq!(b.context_management.clear_tool_uses_20250919, YES);
    assert_eq!(b.context_management.compact_20260112, YES);

    // One edit; the other unknown is false, the compaction edit too.
    let b = anthropic_blocks(&with_editing(Some(true), &["clear_tool_uses_20250919"]));
    assert_eq!(b.context_management.clear_tool_uses_20250919, YES);
    assert_eq!(b.context_management.clear_thinking_20251015, NO);
    assert_eq!(b.context_management.compact_20260112, NO);

    // An Anthropic route whose catalog says nothing: the field still reaches
    // the upstream, but nothing says the model takes it, and Anthropic's
    // schema has no unknown: false, as for an unpublished thinking type.
    let b = anthropic_blocks(&with_editing(None, &[]));
    assert!(!b.context_management.supported);
    assert_eq!(b.context_management.clear_tool_uses_20250919, NO);

    // The provider says the model does not take it.
    let b = anthropic_blocks(&with_editing(Some(false), &["clear_tool_uses_20250919"]));
    assert!(!b.context_management.supported);
    assert_eq!(b.context_management.clear_tool_uses_20250919, NO);

    // Off the Messages route nothing of it is served.
    let caps = ModelCapabilities {
        endpoints: strings(&["/v1/chat/completions"]),
        ..with_editing(Some(true), &["clear_tool_uses_20250919"])
    };
    assert!(!anthropic_blocks(&caps).context_management.supported);
}

/// The on-demand `compaction` block: true exactly where the Anthropic
/// route's catalog names a compaction type for the model, whatever
/// `context_management.supported` says (it is a field of its own).
#[test]
fn compaction_follows_the_anthropic_routes_published_types() {
    let b = anthropic_blocks(&with_compaction(None, &[], &["summarize"]));
    assert!(b.compaction.supported);
    assert_eq!(b.compaction.summarize, YES);

    // A newer type alone: the block says yes, summarize no.
    let b = anthropic_blocks(&with_compaction(Some(true), &[], &["summarize_v2"]));
    assert!(b.compaction.supported);
    assert_eq!(b.compaction.summarize, NO);

    // Unpublished is false, as everywhere in Anthropic's blocks.
    let b = anthropic_blocks(&with_editing(Some(true), &["compact_20260112"]));
    assert!(!b.compaction.supported);
    assert_eq!(b.compaction.summarize, NO);

    // Not an Anthropic route (no `context_editing`): false.
    assert!(!anthropic_blocks(&chat()).compaction.supported);

    // Off the Messages route nothing of it is served.
    let caps = ModelCapabilities {
        endpoints: strings(&["/v1/chat/completions"]),
        ..with_compaction(Some(true), &[], &["summarize"])
    };
    assert!(!anthropic_blocks(&caps).compaction.supported);
}

#[test]
fn a_model_off_the_messages_route_is_false_in_every_block() {
    let caps = ModelCapabilities {
        task: "tts".into(),
        endpoints: strings(&["/v1/audio/speech"]),
        vision: Some(true),
        reasoning: Some(local_levels()),
        structured_output: Some(StructuredOutputCaps {
            json_schema: Some(true),
            json_object: None,
        }),
        ..chat()
    };
    let v = serde_json::to_value(anthropic_blocks(&caps)).unwrap();
    let mut trues = Vec::new();
    collect_trues(&v, String::new(), &mut trues);
    assert!(trues.is_empty(), "{trues:?}");
}

fn collect_trues(v: &Value, at: String, out: &mut Vec<String>) {
    match v {
        Value::Bool(true) => out.push(at),
        Value::Object(o) => {
            for (k, v) in o {
                collect_trues(v, format!("{at}/{k}"), out);
            }
        }
        _ => {}
    }
}

#[test]
fn the_anthropic_object_is_lmgws_own_plus_anthropics_blocks() {
    let caps = ModelCapabilities {
        vision: Some(true),
        ..with_reasoning(local_levels())
    };
    let own = serde_json::to_value(&caps).unwrap();
    let merged = anthropic_capabilities(&caps);
    for (k, v) in own.as_object().unwrap() {
        assert_eq!(merged.get(k), Some(v), "lmgw's {k} must be unchanged");
    }
    let blocks = [
        "batch",
        "citations",
        "code_execution",
        "compaction",
        "context_management",
        "effort",
        "image_input",
        "pdf_input",
        "server_tools",
        "structured_outputs",
        "thinking",
    ];
    for b in blocks {
        assert!(own.get(b).is_none(), "{b} collides with an lmgw key: {own}");
        assert!(merged.get(b).is_some(), "{b} missing: {merged}");
    }
    assert_eq!(
        merged.as_object().unwrap().len(),
        own.as_object().unwrap().len() + blocks.len()
    );
    assert_eq!(merged["image_input"], json!({"supported": true}));
    assert_eq!(
        merged["compaction"],
        json!({"supported": false, "summarize": {"supported": false}})
    );
    assert_eq!(
        merged["context_management"],
        json!({
            "supported": false,
            "clear_thinking_20251015": {"supported": false},
            "clear_tool_uses_20250919": {"supported": false},
            "compact_20260112": {"supported": false},
        })
    );
    assert_eq!(
        merged["thinking"],
        json!({"supported": true, "types": {
            "adaptive": {"supported": true},
            "between_tools": {"supported": false},
            "disabled": {"supported": true},
            "enabled": {"supported": false},
        }})
    );
}
