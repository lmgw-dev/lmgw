//! The reasoning control in a chat body (§9.1, model-capabilities §5.3):
//! off, on, a level, a budget, both, the normalisations, and every way a
//! client's own reasoning keys ride along in passthrough — a level while off,
//! the template kwargs' deep merge, OpenRouter's `reasoning` object, and
//! llama-server's older `thinking_budget_tokens` (I2 and I3 re-bless only
//! the `thinking_budget_*` cases and the budget ones).

use lmgw_core::config::Upstream;
use serde_json::{json, Value};

use super::{chat_request, ir, msg, params, Case};

/// A one-turn request under the control `control` (a `ReasoningControl`'s
/// serde shape), with `passthrough` riding along.
fn reasoning(up: &Upstream, control: Value, passthrough: Value, stream: bool) -> Value {
    let mut r = ir(json!([msg("user", "Why is the sky blue?")]));
    r.passthrough = passthrough.as_object().cloned().unwrap();
    let p = if control.is_null() {
        params(json!({}))
    } else {
        params(json!({"reasoning": control}))
    };
    chat_request(up, &r, &p, stream)
}

/// OpenRouter's object, as a client of that dialect sends it.
fn openrouter() -> Value {
    json!({"reasoning": {"effort": "low", "exclude": true, "max_tokens": 100}})
}

pub(super) fn cases() -> Vec<Case> {
    vec![
        // --- The control alone --------------------------------------------
        Case {
            name: "reasoning_unset",
            run: |up| reasoning(up, Value::Null, json!({}), false),
        },
        Case {
            name: "reasoning_off",
            run: |up| reasoning(up, json!({"enabled": false}), json!({}), false),
        },
        Case {
            name: "reasoning_on",
            run: |up| reasoning(up, json!({"enabled": true}), json!({}), false),
        },
        Case {
            name: "reasoning_effort",
            run: |up| reasoning(up, json!({"effort": "high"}), json!({}), false),
        },
        Case {
            name: "reasoning_budget",
            run: |up| reasoning(up, json!({"budget_tokens": 512}), json!({}), false),
        },
        Case {
            name: "reasoning_effort_and_budget",
            run: |up| {
                reasoning(
                    up,
                    json!({"effort": "low", "budget_tokens": 256}),
                    json!({}),
                    false,
                )
            },
        },
        Case {
            name: "reasoning_effort_none_is_off",
            run: |up| reasoning(up, json!({"effort": "none"}), json!({}), false),
        },
        Case {
            name: "reasoning_budget_zero_is_off",
            run: |up| reasoning(up, json!({"budget_tokens": 0}), json!({}), false),
        },
        Case {
            name: "reasoning_effort_stream",
            run: |up| reasoning(up, json!({"effort": "medium"}), json!({}), true),
        },
        // --- A passthrough level -------------------------------------------
        Case {
            name: "reasoning_off_with_passthrough_level",
            run: |up| {
                reasoning(
                    up,
                    json!({"enabled": false}),
                    json!({"reasoning_effort": "high"}),
                    false,
                )
            },
        },
        Case {
            name: "reasoning_effort_over_passthrough_level",
            run: |up| {
                reasoning(
                    up,
                    json!({"effort": "high"}),
                    json!({"reasoning_effort": "low"}),
                    false,
                )
            },
        },
        Case {
            name: "reasoning_on_keeps_passthrough_level",
            run: |up| {
                reasoning(
                    up,
                    json!({"enabled": true}),
                    json!({"reasoning_effort": "low"}),
                    false,
                )
            },
        },
        // --- Template kwargs -----------------------------------------------
        Case {
            name: "reasoning_kwargs_deep_merge",
            run: |up| {
                reasoning(
                    up,
                    json!({"enabled": true}),
                    json!({"chat_template_kwargs": {"preserve_thinking": true}}),
                    false,
                )
            },
        },
        Case {
            name: "reasoning_kwargs_overridden",
            run: |up| {
                reasoning(
                    up,
                    json!({"enabled": false}),
                    json!({"chat_template_kwargs": {"enable_thinking": true, "x": 1}}),
                    false,
                )
            },
        },
        Case {
            name: "reasoning_kwargs_without_control",
            run: |up| {
                reasoning(
                    up,
                    Value::Null,
                    json!({"chat_template_kwargs": {"enable_thinking": false}}),
                    false,
                )
            },
        },
        // --- OpenRouter's object -------------------------------------------
        Case {
            name: "reasoning_object_with_effort",
            run: |up| reasoning(up, json!({"effort": "high"}), openrouter(), false),
        },
        Case {
            name: "reasoning_object_off",
            run: |up| reasoning(up, json!({"enabled": false}), openrouter(), false),
        },
        Case {
            name: "reasoning_object_on",
            run: |up| reasoning(up, json!({"enabled": true}), openrouter(), false),
        },
        Case {
            name: "reasoning_object_with_budget",
            run: |up| reasoning(up, json!({"budget_tokens": 512}), openrouter(), false),
        },
        Case {
            name: "reasoning_object_without_control",
            run: |up| reasoning(up, Value::Null, openrouter(), false),
        },
        // --- llama-server's older budget name, and the newer one -----------
        Case {
            name: "thinking_budget_passthrough_with_budget",
            run: |up| {
                reasoning(
                    up,
                    json!({"budget_tokens": 512}),
                    json!({"thinking_budget_tokens": 100}),
                    false,
                )
            },
        },
        Case {
            name: "thinking_budget_passthrough_with_effort",
            run: |up| {
                reasoning(
                    up,
                    json!({"effort": "high"}),
                    json!({"thinking_budget_tokens": 100}),
                    false,
                )
            },
        },
        Case {
            name: "thinking_budget_passthrough_off",
            run: |up| {
                reasoning(
                    up,
                    json!({"enabled": false}),
                    json!({"thinking_budget_tokens": 100}),
                    false,
                )
            },
        },
        Case {
            name: "thinking_budget_passthrough_without_control",
            run: |up| {
                reasoning(
                    up,
                    Value::Null,
                    json!({"thinking_budget_tokens": 100}),
                    false,
                )
            },
        },
        Case {
            name: "reasoning_budget_passthrough_with_budget",
            run: |up| {
                reasoning(
                    up,
                    json!({"budget_tokens": 512}),
                    json!({"reasoning_budget_tokens": 100}),
                    false,
                )
            },
        },
    ]
}
