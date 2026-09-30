//! `capabilities::apply_owner_override` — the owner-override merge rules of
//! `docs/design/2026-09-17-model-capabilities-design.md` §7.
//!
//! Every case builds a hand-written [`Derived`] (what a builder in
//! `capabilities::mod` would have produced) and a JSON override, and checks
//! the merged result — the function is pure, so none of this touches a GGUF,
//! a catalog or the database.

use lmgw_core::capabilities::{
    apply_owner_override, Derived, ModelCapabilities, ReasoningCaps, StructuredOutputCaps,
    ToolCallCaps,
};
use serde_json::json;

/// The design §2.1 example row: Qwen3.8 served with reasoning on at `low`,
/// native tool calls, both structured-output shapes.
fn derived_row() -> Derived {
    let caps = ModelCapabilities {
        task: "chat".to_string(),
        endpoints: vec![
            "/v1/chat/completions".to_string(),
            "/v1/messages".to_string(),
            "/v1/responses".to_string(),
            "/v1/completions".to_string(),
        ],
        input_modalities: Some(vec!["text".to_string(), "image".to_string()]),
        output_modalities: Some(vec!["text".to_string()]),
        vision: Some(true),
        reasoning: Some(ReasoningCaps {
            kind: "levels".to_string(),
            enabled: Some(true),
            levels: vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
            ],
            default: Some("low".to_string()),
            can_disable: Some(true),
            budget_tokens: None,
            preserve_history: Some(true),
            control: vec![
                "x-lmgw-reasoning".to_string(),
                "x-lmgw-reasoning-effort".to_string(),
            ],
        }),
        tool_calls: Some(ToolCallCaps {
            kind: "native".to_string(),
            parallel: Some(true),
            format: Some("qwen-xml".to_string()),
        }),
        structured_output: Some(StructuredOutputCaps {
            json_schema: Some(true),
            json_object: Some(true),
        }),
        source: "gguf+config".to_string(),
    };
    Derived {
        capabilities: Some(caps),
        max_output_tokens: Some(32768),
        notes: vec!["Reasoning is ON by default at effort 'low'.".to_string()],
        created: None,
    }
}

/// The minimal base `ops::parse_capabilities_override` shape-checks a write
/// against, when there is no real derived row to merge onto (a bare `chat`
/// object rather than `Derived::default()`, whose `capabilities` is `None`).
fn minimal_base() -> Derived {
    Derived {
        capabilities: Some(ModelCapabilities {
            task: "chat".to_string(),
            endpoints: Vec::new(),
            source: "gguf+config".to_string(),
            ..Default::default()
        }),
        ..Derived::default()
    }
}

// ---------------------------------------------------------------------------
// Deep merge over an existing `capabilities` object
// ---------------------------------------------------------------------------

#[test]
fn a_nested_reasoning_key_is_replaced_while_its_siblings_survive() {
    let out = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "reasoning": { "kind": "toggle" } } }),
    )
    .unwrap();
    let r = out.capabilities.unwrap().reasoning.unwrap();
    assert_eq!(r.kind, "toggle", "the named key changed");
    // Everything else `reasoning` carried is untouched — a deep merge, not a
    // whole-object replace.
    assert_eq!(r.enabled, Some(true));
    assert_eq!(r.levels, vec!["low", "medium", "high", "xhigh"]);
    assert_eq!(r.default.as_deref(), Some("low"));
    assert_eq!(r.can_disable, Some(true));
    assert_eq!(r.preserve_history, Some(true));
}

#[test]
fn a_top_level_sibling_the_override_never_mentions_survives() {
    let out = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "vision": false } }),
    )
    .unwrap();
    let caps = out.capabilities.unwrap();
    assert_eq!(caps.vision, Some(false));
    // `tool_calls`, `structured_output`, `task`, `endpoints` were never named.
    assert_eq!(caps.tool_calls.unwrap().kind, "native");
    assert!(caps.structured_output.unwrap().json_schema.unwrap());
    assert_eq!(caps.task, "chat");
    assert_eq!(caps.endpoints.len(), 4);
}

#[test]
fn null_deletes_a_key_rather_than_setting_it_to_a_default() {
    let out = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "vision": null } }),
    )
    .unwrap();
    assert_eq!(out.capabilities.unwrap().vision, None);
}

#[test]
fn an_array_is_replaced_whole_not_merged_element_by_element() {
    let out = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "reasoning": { "levels": ["low", "high"] } } }),
    )
    .unwrap();
    let r = out.capabilities.unwrap().reasoning.unwrap();
    assert_eq!(r.levels, vec!["low", "high"]);
}

#[test]
fn source_becomes_owner_whenever_the_capabilities_object_is_touched() {
    let out = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "vision": false } }),
    )
    .unwrap();
    assert_eq!(out.capabilities.unwrap().source, "owner");
}

#[test]
fn capabilities_absent_from_the_override_leaves_the_source_alone() {
    let out = apply_owner_override(derived_row(), &json!({ "max_output_tokens": 1024 })).unwrap();
    assert_eq!(out.capabilities.unwrap().source, "gguf+config");
}

// ---------------------------------------------------------------------------
// max_output_tokens / notes
// ---------------------------------------------------------------------------

#[test]
fn max_output_tokens_can_be_set_and_cleared() {
    let out = apply_owner_override(derived_row(), &json!({ "max_output_tokens": 65536 })).unwrap();
    assert_eq!(out.max_output_tokens, Some(65536));

    let cleared =
        apply_owner_override(derived_row(), &json!({ "max_output_tokens": null })).unwrap();
    assert_eq!(cleared.max_output_tokens, None);
}

#[test]
fn notes_are_appended_not_replaced() {
    let out = apply_owner_override(
        derived_row(),
        &json!({ "notes": ["hand-verified against a live probe"] }),
    )
    .unwrap();
    assert_eq!(
        out.notes,
        vec![
            "Reasoning is ON by default at effort 'low'.".to_string(),
            "hand-verified against a live probe".to_string(),
        ]
    );
}

// ---------------------------------------------------------------------------
// Override onto a `None` derived object (an unreadable GGUF)
// ---------------------------------------------------------------------------

#[test]
fn an_override_onto_a_none_derived_object_becomes_the_whole_object() {
    let unreadable = Derived {
        capabilities: None,
        max_output_tokens: None,
        notes: vec!["weights.gguf could not be read".to_string()],
        created: None,
    };
    let out = apply_owner_override(
        unreadable,
        &json!({
            "capabilities": {
                "task": "chat",
                "endpoints": ["/v1/chat/completions"],
                "source": "catalog",
            }
        }),
    )
    .unwrap();
    let caps = out
        .capabilities
        .expect("the override supplied a whole object");
    assert_eq!(caps.task, "chat");
    assert_eq!(caps.endpoints, vec!["/v1/chat/completions".to_string()]);
    assert_eq!(caps.source, "owner");
    // The note about the unreadable file is not silently dropped.
    assert_eq!(
        out.notes,
        vec!["weights.gguf could not be read".to_string()]
    );
}

#[test]
fn an_override_onto_a_none_derived_object_without_task_is_refused() {
    let unreadable = Derived::default();
    let err = apply_owner_override(
        unreadable,
        &json!({ "capabilities": { "endpoints": ["/v1/chat/completions"] } }),
    )
    .unwrap_err();
    assert!(err.contains("task"), "{err}");
}

// ---------------------------------------------------------------------------
// Malformed shapes: Err naming the offending key
// ---------------------------------------------------------------------------

#[test]
fn a_non_object_override_is_refused() {
    let err = apply_owner_override(derived_row(), &json!(["not", "an", "object"])).unwrap_err();
    assert!(err.contains("object"), "{err}");
}

#[test]
fn an_unknown_top_level_key_is_refused_and_named() {
    let err = apply_owner_override(derived_row(), &json!({ "bogus_key": 1 })).unwrap_err();
    assert!(err.contains("bogus_key"), "{err}");
}

#[test]
fn a_non_object_capabilities_value_is_refused_and_named() {
    let err = apply_owner_override(derived_row(), &json!({ "capabilities": "not an object" }))
        .unwrap_err();
    assert!(err.contains("capabilities"), "{err}");
}

#[test]
fn a_capabilities_shape_that_fails_to_deserialise_is_refused() {
    // `reasoning.kind` is a `String` field; a number cannot become one.
    let err = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "reasoning": { "kind": 123 } } }),
    )
    .unwrap_err();
    assert!(err.contains("capabilities"), "{err}");
}

#[test]
fn a_non_array_notes_value_is_refused() {
    let err = apply_owner_override(derived_row(), &json!({ "notes": "not an array" })).unwrap_err();
    assert!(err.contains("notes"), "{err}");
}

#[test]
fn a_non_numeric_max_output_tokens_is_refused() {
    let err =
        apply_owner_override(derived_row(), &json!({ "max_output_tokens": "a lot" })).unwrap_err();
    assert!(err.contains("max_output_tokens"), "{err}");
}

/// The shape check `ops::parse_capabilities_override` runs at write time,
/// against a minimal stand-in base rather than a real derived row — proven
/// here directly against the function it wraps, since that helper itself is
/// not part of the pure `capabilities` module's public surface.
#[test]
fn the_minimal_write_time_base_accepts_a_well_formed_override() {
    let out = apply_owner_override(
        minimal_base(),
        &json!({ "capabilities": { "input_modalities": ["text", "image"] } }),
    )
    .unwrap();
    let caps = out.capabilities.unwrap();
    assert_eq!(caps.task, "chat");
    assert_eq!(caps.source, "owner");
    assert_eq!(
        caps.input_modalities,
        Some(vec!["text".to_string(), "image".to_string()])
    );
}

// ---------------------------------------------------------------------------
// Validation: misspelled keys and out-of-vocabulary values (review F6)
// ---------------------------------------------------------------------------

/// A misspelled key inside the capabilities object is refused and named.
/// Without `deny_unknown_fields` serde drops it silently, so an owner writing
/// `visionn: true` would see the override "succeed" and change nothing.
#[test]
fn a_misspelled_capability_key_is_refused_and_named() {
    let err = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "visionn": true } }),
    )
    .unwrap_err();
    assert!(err.contains("visionn"), "{err}");

    let err = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "tool_calls": { "kindd": "native" } } }),
    )
    .unwrap_err();
    assert!(err.contains("kindd"), "{err}");
}

/// A *value* outside the vocabulary is refused too: a consumer switches on
/// `reasoning.kind` and `tool_calls.kind`, so a typo there silently disables a
/// feature on the client side rather than erroring anywhere.
#[test]
fn an_out_of_vocabulary_kind_is_refused_and_named() {
    let err = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "reasoning": { "kind": "levelz" } } }),
    )
    .unwrap_err();
    assert!(err.contains("reasoning.kind"), "{err}");
    assert!(err.contains("levelz"), "{err}");

    let err = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "tool_calls": { "kind": "structured" } } }),
    )
    .unwrap_err();
    assert!(err.contains("tool_calls.kind"), "{err}");

    // …and `text`, which an owner legitimately sets by hand, is accepted.
    let out = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "tool_calls": { "kind": "text" } } }),
    )
    .unwrap();
    assert_eq!(out.capabilities.unwrap().tool_calls.unwrap().kind, "text");
}

#[test]
fn an_unknown_task_or_modality_is_refused_and_named() {
    let err = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "task": "chatt" } }),
    )
    .unwrap_err();
    assert!(err.contains("task"), "{err}");
    assert!(err.contains("chatt"), "{err}");

    let err = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "input_modalities": ["text", "images"] } }),
    )
    .unwrap_err();
    assert!(err.contains("input_modalities"), "{err}");
    assert!(err.contains("images"), "{err}");

    // An audio.cpp task name is a task, and `pdf` (which the Kilo catalogs
    // publish) is a modality.
    let out = apply_owner_override(
        derived_row(),
        &json!({ "capabilities": { "task": "diar", "input_modalities": ["text", "pdf"] } }),
    )
    .unwrap();
    assert_eq!(out.capabilities.unwrap().task, "diar");
}

/// `source` describes the capabilities object only (§7), so a hand-set cap
/// says so in a note — otherwise it is indistinguishable from a number read
/// off the model.
#[test]
fn a_hand_set_max_output_tokens_is_attributed_in_a_note() {
    let out = apply_owner_override(derived_row(), &json!({ "max_output_tokens": 65536 })).unwrap();
    assert_eq!(out.max_output_tokens, Some(65536));
    assert_eq!(
        out.notes.last().map(String::as_str),
        Some(
            "max_output_tokens was set by the owner (capabilities_override), not read from the \
             model."
        )
    );
    assert_eq!(
        out.capabilities.unwrap().source,
        "gguf+config",
        "the capabilities object itself was not touched"
    );

    // Clearing one leaves nothing to attribute.
    let cleared =
        apply_owner_override(derived_row(), &json!({ "max_output_tokens": null })).unwrap();
    assert!(
        !cleared.notes.iter().any(|n| n.contains("set by the owner")),
        "{:?}",
        cleared.notes
    );
}
