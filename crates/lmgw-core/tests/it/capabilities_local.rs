//! `capabilities::for_local_row` — the GGUF+config derivation of a local
//! llama.cpp chat row, per
//! `docs/design/2026-09-17-model-capabilities-design.md` §3.2–§3.4
//! and §3.6.
//!
//! Every case is a hand-built row plus the real chat template of the model it
//! claims to be (the checked-in fixtures under `tests/fixtures/chat_templates/`,
//! the same ones `gguf::TemplateSignals` is tested against), so a rule that
//! stops matching the templates on disk fails here rather than in production.
//! The builder is pure, so none of this touches the filesystem.

use lmgw_core::capabilities::{self, ProjectorStatus};
use lmgw_core::config::{LlamaParams, LocalModel};
use lmgw_core::gguf::{ModelSummary, TemplateSignals};
use lmgw_core::ir::{Params, ReasoningControl};

const QWEN38: &str = include_str!("../fixtures/chat_templates/qwen3.8.jinja");
const GEMMA4: &str = include_str!("../fixtures/chat_templates/gemma4.jinja");
const MEDGEMMA: &str = include_str!("../fixtures/chat_templates/medgemma.jinja");
const COHERE_NORTH: &str = include_str!("../fixtures/chat_templates/cohere-north.jinja");
const MUSE_GLIMMER: &str = include_str!("../fixtures/chat_templates/muse-glimmer.jinja");

fn weights(tpl: &str) -> ModelSummary {
    ModelSummary {
        architecture: Some("qwen3".into()),
        has_chat_template: true,
        chat_template: Some(tpl.to_string()),
        signals: Some(TemplateSignals::from_template(tpl)),
        ..Default::default()
    }
}

fn mmproj(vision: Option<bool>, audio: Option<bool>) -> ModelSummary {
    ModelSummary {
        general_type: Some("mmproj".into()),
        is_mmproj: true,
        has_vision_encoder: vision,
        has_audio_encoder: audio,
        ..Default::default()
    }
}

fn row(model_id: &str, params: LlamaParams, args: &[&str]) -> LocalModel {
    LocalModel {
        id: 1,
        model_id: model_id.into(),
        gguf_path: format!("{model_id}/weights.gguf"),
        params,
        args: args.iter().map(|s| s.to_string()).collect(),
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

/// The row the design's §2.1 example is written against: Qwen3.8 served with
/// `--reasoning on --reasoning-effort low --reasoning-preserve`.
fn qwen38_row() -> LocalModel {
    row(
        "qwen3.8-27b-reason",
        LlamaParams {
            reasoning: Some("on".into()),
            reasoning_effort: Some("low".into()),
            reasoning_preserve: Some(true),
            ..Default::default()
        },
        &[],
    )
}

fn derive(model: &LocalModel, tpl: &str) -> capabilities::Derived {
    capabilities::for_local_row(
        model,
        Some(&weights(tpl)),
        None,
        None,
        ProjectorStatus::None,
        None,
    )
}

// ---------------------------------------------------------------------------
// Reasoning (§3.2)
// ---------------------------------------------------------------------------

#[test]
fn qwen38_reasoning_on_at_low_is_levels() {
    let d = derive(&qwen38_row(), QWEN38);
    let caps = d.capabilities.expect("capabilities");
    assert_eq!(caps.task, "chat");
    assert_eq!(
        caps.endpoints,
        [
            "/v1/chat/completions",
            "/v1/messages",
            "/v1/responses",
            "/v1/completions"
        ]
    );
    assert_eq!(caps.source, "gguf+config");

    let r = caps.reasoning.expect("reasoning");
    assert_eq!(r.kind, "levels");
    assert_eq!(r.enabled, Some(true));
    assert_eq!(r.levels, ["low", "medium", "high", "xhigh"]);
    assert_eq!(r.default.as_deref(), Some("low"));
    assert_eq!(r.can_disable, Some(true));
    assert_eq!(r.preserve_history, Some(true));
    assert_eq!(r.budget_tokens, None);
    assert_eq!(
        r.control,
        [
            "x-lmgw-reasoning",
            "x-lmgw-reasoning-effort",
            "reasoning_effort",
            "chat_template_kwargs.enable_thinking"
        ],
        "the budget header is not listed for local rows (§5.3 rev 2)"
    );

    let t = caps.tool_calls.expect("tool_calls");
    assert_eq!(t.kind, "native");
    assert_eq!(t.parallel, Some(true));
    assert_eq!(t.format.as_deref(), Some("qwen-xml"));

    let s = caps.structured_output.expect("structured_output");
    assert_eq!(s.json_schema, Some(true));
    assert_eq!(s.json_object, Some(true));
}

/// `--reasoning off` on a `levels` template stays `levels`: the request can
/// turn it back on, which is exactly what the kind is meant to convey (§3.2).
#[test]
fn qwen38_reasoning_off_keeps_levels_and_reports_disabled() {
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            reasoning: Some("off".into()),
            ..Default::default()
        },
        &[],
    );
    let r = derive(&model, QWEN38)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(r.kind, "levels");
    assert_eq!(r.enabled, Some(false));
    assert_eq!(r.levels, ["low", "medium", "high", "xhigh"]);
    assert_eq!(
        r.default.as_deref(),
        Some("xhigh"),
        "no --reasoning-effort on the row ⇒ the template's own default"
    );
}

/// A zero budget is reported as `enabled: false`, never as a budget (§3.2).
#[test]
fn reasoning_budget_zero_is_disabled_not_a_budget() {
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            reasoning: Some("on".into()),
            reasoning_budget: Some(0),
            ..Default::default()
        },
        &[],
    );
    let r = derive(&model, QWEN38)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(r.enabled, Some(false), "budget 0 beats --reasoning on");
    assert_eq!(r.budget_tokens, None);
}

#[test]
fn positive_reasoning_budget_is_published() {
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            reasoning_budget: Some(8192),
            ..Default::default()
        },
        &[],
    );
    let r = derive(&model, QWEN38)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(r.budget_tokens, Some(8192));
    assert_eq!(
        r.enabled,
        Some(true),
        "nothing configured ⇒ the template's enable_thinking default (true)"
    );
}

#[test]
fn chat_template_kwargs_enable_thinking_false_disables() {
    let mut kwargs = serde_json::Map::new();
    kwargs.insert("enable_thinking".into(), serde_json::Value::Bool(false));
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            chat_template_kwargs: kwargs,
            ..Default::default()
        },
        &[],
    );
    let r = derive(&model, QWEN38)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(r.enabled, Some(false));
}

/// `reasoning_effort` written into the freeform kwargs is the same template
/// variable as `--reasoning-effort` (`LlamaParams::fold_reasoning_effort`), so
/// it is the published default even on a row that never went through the
/// loader's fold.
#[test]
fn reasoning_effort_from_chat_template_kwargs_is_the_default() {
    let mut kwargs = serde_json::Map::new();
    kwargs.insert("reasoning_effort".into(), "medium".into());
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            chat_template_kwargs: kwargs,
            ..Default::default()
        },
        &[],
    );
    let r = derive(&model, QWEN38)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(r.default.as_deref(), Some("medium"));
}

/// gemma4 reads `enable_thinking` but no effort variable ⇒ `toggle`, and its
/// template's own `| default(false)` is the state when the row says nothing.
#[test]
fn gemma4_unconfigured_is_a_toggle_defaulting_off() {
    let model = row("gemma4-e4b", LlamaParams::default(), &[]);
    let caps = derive(&model, GEMMA4).capabilities.unwrap();
    let r = caps.reasoning.unwrap();
    assert_eq!(r.kind, "toggle");
    assert_eq!(r.enabled, Some(false));
    assert!(r.levels.is_empty());
    assert_eq!(r.default, None, "a toggle template reads no effort level");
    assert_eq!(r.can_disable, Some(true));
    // Only the switch pair: nothing this template reads is filled from a
    // request's `reasoning_effort`, so advertising the effort controls would
    // point a client at a header that changes nothing.
    assert_eq!(
        r.control,
        ["x-lmgw-reasoning", "chat_template_kwargs.enable_thinking"]
    );
    assert_eq!(caps.tool_calls.unwrap().format.as_deref(), Some("gemma"));
}

#[test]
fn gemma4_with_reasoning_on_is_enabled() {
    let model = row(
        "gemma4-e4b-reason",
        LlamaParams {
            reasoning: Some("on".into()),
            ..Default::default()
        },
        &[],
    );
    let r = derive(&model, GEMMA4)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(r.kind, "toggle");
    assert_eq!(r.enabled, Some(true));
}

/// Cohere North compares `reasoning_effort` against `"none"` only ⇒ `toggle`
/// with an empty level list, and no `enable_thinking` to switch it off with.
#[test]
fn cohere_north_is_a_toggle_that_cannot_be_disabled() {
    let model = row("north", LlamaParams::default(), &[]);
    let r = derive(&model, COHERE_NORTH)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(r.kind, "toggle");
    assert!(r.levels.is_empty());
    assert_eq!(r.can_disable, Some(false));
    // It *does* read `reasoning_effort`, which llama-server fills from a
    // request — so the effort pair is published and the `enable_thinking`
    // pair, which this template never reads, is not.
    assert_eq!(r.control, ["x-lmgw-reasoning-effort", "reasoning_effort"]);
}

/// Muse-Glimmer's template reads only its own `reasoning_strength`, a name
/// llama-server never fills from a request. Nothing a client sends can change
/// how it thinks, so it is `fixed` — the old answer, `toggle`, advertised a
/// switch wired to nothing.
#[test]
fn a_template_whose_only_effort_var_is_unreachable_is_fixed() {
    let model = row("muse-glimmer-30b", LlamaParams::default(), &[]);
    let d = derive(&model, MUSE_GLIMMER);
    let caps = d.capabilities.unwrap();
    let r = caps.reasoning.unwrap();
    assert_eq!(r.kind, "fixed");
    assert_eq!(r.enabled, Some(true), "its markers say it thinks");
    assert!(r.control.is_empty());
    assert!(r.levels.is_empty());
    assert_eq!(r.can_disable, None);
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("reads reasoning_strength, which lmgw does not set")),
        "the note must name the variable lmgw cannot reach: {:?}",
        d.notes
    );
}

/// …and a row that reads a reachable effort variable gets no such note.
#[test]
fn a_reachable_effort_variable_earns_no_unreachable_note() {
    let d = derive(&qwen38_row(), QWEN38);
    assert!(
        !d.notes.iter().any(|n| n.contains("lmgw does not set")),
        "{:?}",
        d.notes
    );
}

/// A template that renders tools in a syntax matching none of the known
/// markers is `text`: llama-server has no parser for it, so the calls may come
/// back as prose. Calling it `native` would promise structured `tool_calls`
/// that never arrive.
#[test]
fn tools_in_an_unrecognised_syntax_are_text_not_native() {
    let model = row("muse-glimmer-30b", LlamaParams::default(), &[]);
    let t = derive(&model, MUSE_GLIMMER)
        .capabilities
        .unwrap()
        .tool_calls
        .unwrap();
    assert_eq!(t.kind, "text");
    assert_eq!(t.format.as_deref(), Some("unknown"));
    assert_eq!(t.parallel, Some(true));
}

/// `chat_template_kwargs.preserve_thinking` reaches the same template variable
/// as `--reasoning-preserve`, so it is a stated fact and beats the template's
/// mere mention of it.
#[test]
fn preserve_thinking_kwarg_beats_the_template_default() {
    let mut kwargs = serde_json::Map::new();
    kwargs.insert("preserve_thinking".into(), serde_json::Value::Bool(false));
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            chat_template_kwargs: kwargs.clone(),
            ..Default::default()
        },
        &[],
    );
    let r = derive(&model, QWEN38)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(
        r.preserve_history,
        Some(false),
        "the template reads preserve_thinking, but the row turned it off"
    );

    // The flag still wins over the kwarg — one is a CLI argument llama-server
    // parses itself, the other a template variable.
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            chat_template_kwargs: kwargs,
            reasoning_preserve: Some(true),
            ..Default::default()
        },
        &[],
    );
    let r = derive(&model, QWEN38)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(r.preserve_history, Some(true));
}

/// medgemma's template says nothing at all about thinking or tools.
#[test]
fn medgemma_is_fixed_off_with_no_tools() {
    let model = row("medgemma-27b", LlamaParams::default(), &[]);
    let caps = derive(&model, MEDGEMMA).capabilities.unwrap();
    let r = caps.reasoning.unwrap();
    assert_eq!(r.kind, "fixed");
    assert_eq!(r.enabled, Some(false));
    assert!(r.control.is_empty(), "nothing to control on a fixed model");
    assert!(r.levels.is_empty());
    assert_eq!(r.can_disable, None);

    let t = caps.tool_calls.unwrap();
    assert_eq!(t.kind, "none");
    assert_eq!(t.parallel, None);
    assert_eq!(t.format, None);
}

/// Thinking markers but no variable to switch them: the template always
/// thinks (§3.2's last branch).
#[test]
fn always_thinking_template_is_fixed_on() {
    let tpl = "{% for m in messages %}<think>{{ m.content }}</think>{% endfor %}";
    let model = row("always-thinks", LlamaParams::default(), &[]);
    let r = derive(&model, tpl).capabilities.unwrap().reasoning.unwrap();
    assert_eq!(r.kind, "fixed");
    assert_eq!(r.enabled, Some(true));
    assert!(r.control.is_empty());
}

/// …and §3.2's gloss that a zero budget is its only brake is applied, not just
/// noted: the object must not claim such a row thinks.
#[test]
fn always_thinking_template_with_budget_zero_is_fixed_off() {
    let tpl = "{% for m in messages %}<think>{{ m.content }}</think>{% endfor %}";
    let model = row(
        "always-thinks",
        LlamaParams {
            reasoning_budget: Some(0),
            ..Default::default()
        },
        &[],
    );
    let r = derive(&model, tpl).capabilities.unwrap().reasoning.unwrap();
    assert_eq!(r.kind, "fixed");
    assert_eq!(r.enabled, Some(false));
    assert_eq!(r.budget_tokens, None);
}

/// `--no-reasoning-preserve` is a stated fact and beats the template's
/// `preserve_thinking` reference.
#[test]
fn preserve_history_prefers_the_configured_value() {
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            reasoning_preserve: Some(false),
            ..Default::default()
        },
        &[],
    );
    let r = derive(&model, QWEN38)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(r.preserve_history, Some(false));

    // medgemma's template reads no `preserve_thinking` and the row sets
    // nothing ⇒ absent, not `false`.
    let plain = row("medgemma-27b", LlamaParams::default(), &[]);
    let r = derive(&plain, MEDGEMMA)
        .capabilities
        .unwrap()
        .reasoning
        .unwrap();
    assert_eq!(r.preserve_history, None);
}

// ---------------------------------------------------------------------------
// The chat-template override (§3.1)
// ---------------------------------------------------------------------------

/// `--chat-template-file` is what llama-server renders, so its signals win
/// over the ones embedded in the weights.
#[test]
fn chat_template_file_overrides_the_embedded_template() {
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            chat_template_file: Some("templates/medgemma.jinja".into()),
            ..Default::default()
        },
        &[],
    );
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(QWEN38)),
        None,
        Some(MEDGEMMA),
        ProjectorStatus::None,
        None,
    );
    let caps = d.capabilities.unwrap();
    assert_eq!(caps.reasoning.unwrap().kind, "fixed");
    assert_eq!(caps.tool_calls.unwrap().kind, "none");
}

// ---------------------------------------------------------------------------
// Modalities (§3.3)
// ---------------------------------------------------------------------------

#[test]
fn no_projector_is_text_only() {
    let caps = derive(&qwen38_row(), QWEN38).capabilities.unwrap();
    assert_eq!(caps.input_modalities, Some(vec!["text".to_string()]));
    assert_eq!(caps.output_modalities, Some(vec!["text".to_string()]));
    assert_eq!(caps.vision, Some(false));
}

#[test]
fn configured_projector_with_both_encoders_adds_image_and_audio() {
    let model = row(
        "gemma4-e4b-mm",
        LlamaParams {
            mmproj_path: Some("gemma4/mmproj.gguf".into()),
            ..Default::default()
        },
        &[],
    );
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(GEMMA4)),
        Some(&mmproj(Some(true), Some(true))),
        None,
        ProjectorStatus::Configured {
            path: "gemma4/mmproj.gguf",
        },
        None,
    );
    let caps = d.capabilities.unwrap();
    assert_eq!(
        caps.input_modalities,
        Some(vec![
            "text".to_string(),
            "image".to_string(),
            "audio".to_string()
        ])
    );
    assert_eq!(caps.vision, Some(true));
    assert!(
        d.notes.iter().any(|n| n.contains("input_audio")),
        "the audio note must appear when the projector has an audio encoder: {:?}",
        d.notes
    );
}

#[test]
fn vision_only_projector_does_not_claim_audio() {
    let model = row(
        "qwen3vl",
        LlamaParams {
            mmproj_path: Some("qwen3vl/mmproj.gguf".into()),
            ..Default::default()
        },
        &[],
    );
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(QWEN38)),
        Some(&mmproj(Some(true), None)),
        None,
        ProjectorStatus::Configured {
            path: "qwen3vl/mmproj.gguf",
        },
        None,
    );
    let caps = d.capabilities.unwrap();
    assert_eq!(
        caps.input_modalities,
        Some(vec!["text".to_string(), "image".to_string()])
    );
    assert_eq!(caps.vision, Some(true));
}

#[test]
fn unreadable_projector_leaves_modalities_unknown() {
    let model = row(
        "qwen3vl",
        LlamaParams {
            mmproj_path: Some("qwen3vl/mmproj.gguf".into()),
            ..Default::default()
        },
        &[],
    );
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(QWEN38)),
        None,
        None,
        ProjectorStatus::Unreadable {
            path: "qwen3vl/mmproj.gguf",
            err: "unexpected EOF",
        },
        None,
    );
    let caps = d.capabilities.unwrap();
    assert_eq!(caps.input_modalities, None, "unknown, not text-only");
    assert_eq!(caps.vision, None);
    assert!(d
        .notes
        .iter()
        .any(|n| n.contains("qwen3vl/mmproj.gguf") && n.contains("unexpected EOF")));
}

#[test]
fn sibling_projector_that_is_not_configured_is_text_only_plus_a_note() {
    let model = row("qwen3vl-text", LlamaParams::default(), &[]);
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(QWEN38)),
        None,
        None,
        ProjectorStatus::SiblingPresentNotConfigured {
            path: "qwen3vl/mmproj-BF16.gguf",
        },
        None,
    );
    let caps = d.capabilities.unwrap();
    assert_eq!(caps.input_modalities, Some(vec!["text".to_string()]));
    assert_eq!(caps.vision, Some(false));
    assert!(d
        .notes
        .iter()
        .any(|n| n.contains("qwen3vl/mmproj-BF16.gguf") && n.contains("text-only")));
}

/// §3.3: the projector in use is the configured one — the typed field, else a
/// `--mmproj` left in the freeform args. `no_mmproj` does not unset it,
/// because the argv renderer emits `--mmproj` whenever the field is set.
#[test]
fn configured_projector_reads_the_field_then_the_args() {
    let field = row(
        "a",
        LlamaParams {
            mmproj_path: Some("a/mmproj.gguf".into()),
            no_mmproj: true,
            ..Default::default()
        },
        &[],
    );
    assert_eq!(
        capabilities::configured_projector(&field),
        Some("a/mmproj.gguf")
    );

    let spaced = row("b", LlamaParams::default(), &["--mmproj", "b/mmproj.gguf"]);
    assert_eq!(
        capabilities::configured_projector(&spaced),
        Some("b/mmproj.gguf")
    );

    let inline = row("c", LlamaParams::default(), &["--mmproj=c/mmproj.gguf"]);
    assert_eq!(
        capabilities::configured_projector(&inline),
        Some("c/mmproj.gguf")
    );

    let none = row("d", LlamaParams::default(), &["--no-mmproj"]);
    assert_eq!(capabilities::configured_projector(&none), None);

    // `--no-mmproj` claims the flag in the argv renderer, so a `--mmproj` left
    // in the freeform args never reaches the command line — publishing image
    // input for this row would describe a process that does not start.
    let suppressed = row(
        "e",
        LlamaParams {
            no_mmproj: true,
            ..Default::default()
        },
        &["--mmproj", "e/mmproj.gguf"],
    );
    assert_eq!(capabilities::configured_projector(&suppressed), None);
}

// ---------------------------------------------------------------------------
// max_output_tokens, unreadable weights
// ---------------------------------------------------------------------------

#[test]
fn n_predict_is_the_published_output_cap() {
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            n_predict: Some(4096),
            ..Default::default()
        },
        &[],
    );
    let d = derive(&model, QWEN38);
    assert_eq!(d.max_output_tokens, Some(4096));
    assert!(d
        .notes
        .iter()
        .any(|n| n.contains("4096") && n.contains("--n-predict")));
}

#[test]
fn unbounded_n_predict_publishes_no_cap() {
    for n in [None, Some(-1), Some(0)] {
        let model = row(
            "qwen3.8-27b",
            LlamaParams {
                n_predict: n,
                ..Default::default()
            },
            &[],
        );
        let d = derive(&model, QWEN38);
        assert_eq!(d.max_output_tokens, None, "n_predict = {n:?}");
        assert!(d.notes.iter().any(|note| note.contains("No output cap")));
    }
}

// ---------------------------------------------------------------------------
// Ladder note (ladder design §4.4)
// ---------------------------------------------------------------------------

#[test]
fn a_ladder_row_gets_the_rung_count_note() {
    let mut model = row(
        "qwen3.8-27b",
        LlamaParams {
            n_predict: Some(4096),
            ..Default::default()
        },
        &[],
    );
    model.ladder = vec![
        lmgw_core::ladder::Rung {
            gguf_path: "mid.gguf".into(),
            ctx_size: 65536,
        },
        lmgw_core::ladder::Rung {
            gguf_path: "top.gguf".into(),
            ctx_size: 131072,
        },
    ];
    let d = derive(&model, QWEN38);
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("ladder, 3 rungs") && n.contains("switchover")),
        "{:?}",
        d.notes
    );
}

#[test]
fn a_row_without_a_ladder_gets_no_ladder_note() {
    let model = row(
        "qwen3.8-27b",
        LlamaParams {
            n_predict: Some(4096),
            ..Default::default()
        },
        &[],
    );
    assert!(!model.is_ladder());
    let d = derive(&model, QWEN38);
    assert!(
        !d.notes.iter().any(|n| n.contains("ladder,")),
        "{:?}",
        d.notes
    );
}

/// The ladder note fires even when the weights could not be read — it is a
/// fact about the row's own config, not about the GGUF (§4.4).
#[test]
fn the_ladder_note_survives_an_unreadable_gguf() {
    let mut model = row(
        "qwen3.8-27b",
        LlamaParams {
            n_predict: Some(4096),
            ..Default::default()
        },
        &[],
    );
    model.ladder = vec![lmgw_core::ladder::Rung {
        gguf_path: "top.gguf".into(),
        ctx_size: 131072,
    }];
    let d = capabilities::for_local_row(&model, None, None, None, ProjectorStatus::None, None);
    assert!(
        d.notes.iter().any(|n| n.contains("ladder, 2 rungs")),
        "{:?}",
        d.notes
    );
}

#[test]
fn unreadable_weights_yield_no_capabilities_but_name_the_file() {
    let model = row(
        "broken",
        LlamaParams {
            n_predict: Some(2048),
            ..Default::default()
        },
        &[],
    );
    let d = capabilities::for_local_row(&model, None, None, None, ProjectorStatus::None, None);
    assert!(d.capabilities.is_none());
    assert_eq!(
        d.max_output_tokens,
        Some(2048),
        "n_predict comes from the row, not the file"
    );
    assert!(d.notes.iter().any(|n| n.contains("broken/weights.gguf")));
}

#[test]
fn created_is_left_to_the_caller() {
    assert_eq!(derive(&qwen38_row(), QWEN38).created, None);
}

// ---------------------------------------------------------------------------
// Aliases onto a local row (§3.6)
// ---------------------------------------------------------------------------

fn alias_derive(overrides: &Params) -> capabilities::Derived {
    capabilities::for_local_row(
        &qwen38_row(),
        Some(&weights(QWEN38)),
        None,
        None,
        ProjectorStatus::None,
        Some(overrides),
    )
}

#[test]
fn alias_reasoning_overrides_replace_the_rows_defaults() {
    let d = alias_derive(&Params {
        reasoning: Some(ReasoningControl {
            enabled: Some(true),
            effort: Some("high".into()),
            budget_tokens: None,
        }),
        ..Default::default()
    });
    let r = d.capabilities.unwrap().reasoning.unwrap();
    assert_eq!(r.enabled, Some(true));
    assert_eq!(r.default.as_deref(), Some("high"));
}

/// The alias' control is normalised first (§5.1), so an alias that sets effort
/// `none` publishes `enabled: false` rather than an effort level nothing
/// accepts.
#[test]
fn alias_effort_none_disables_rather_than_becoming_a_level() {
    let d = alias_derive(&Params {
        reasoning: Some(ReasoningControl {
            effort: Some("none".into()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let r = d.capabilities.unwrap().reasoning.unwrap();
    assert_eq!(r.enabled, Some(false));
    assert_eq!(
        r.default.as_deref(),
        Some("low"),
        "the row's configured level still describes what 'on' would use"
    );
    assert_eq!(r.budget_tokens, None);
}

/// An alias' `max_tokens` is a per-request default, not a cap llama-server
/// enforces — `max_output_tokens` stays the row's `--n-predict` (absent here).
#[test]
fn alias_max_tokens_is_not_the_published_output_cap() {
    let d = alias_derive(&Params {
        max_tokens: Some(512),
        ..Default::default()
    });
    assert_eq!(d.max_output_tokens, None);
}

/// A `fixed` row has nothing an alias can change per request, so an override
/// must not invent a switch on it.
#[test]
fn alias_overrides_do_not_touch_a_fixed_row() {
    let model = row("medgemma-27b", LlamaParams::default(), &[]);
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(MEDGEMMA)),
        None,
        None,
        ProjectorStatus::None,
        Some(&Params {
            reasoning: Some(ReasoningControl {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
    );
    let r = d.capabilities.unwrap().reasoning.unwrap();
    assert_eq!(r.kind, "fixed");
    assert_eq!(r.enabled, Some(false));
}
