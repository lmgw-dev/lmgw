//! The reasoning control in a llama.cpp chat body (model-capabilities §5.3,
//! llama egress design §3.1, §7): the llama half of what the OpenAI egress
//! did by kind, plus the budget under both of its names (I3).
//!
//! **One switch on every build** (decision 15, §7):
//! `chat_template_kwargs.enable_thinking` turns thinking on and off, and off
//! is that kwarg alone, never `reasoning_effort: "none"`. On image
//! `official-67672dc5b` a per-request `"none"` and the budgets were ignored,
//! and only the kwarg switched; newer builds take `"none"`, but the kwarg
//! still switches both ways there and on ik_llama.cpp. Keying on `build_info`
//! would need a table of builds that is never finished.

use serde_json::{json, Map, Value};

use crate::egress::openai_wire::ReasoningStep;
use crate::ir::{ChatRequest, ReasoningControl};

/// The llama.cpp egress's [`ReasoningStep`]: [`apply_reasoning`] before the
/// passthrough loop, [`reconcile_reasoning_passthrough`] after it.
pub(super) struct LlamaReasoning;

impl ReasoningStep for LlamaReasoning {
    fn apply(&self, body: &mut Map<String, Value>, ir: &ChatRequest, c: &ReasoningControl) {
        apply_reasoning(body, ir, c);
    }

    fn reconcile(&self, body: &mut Map<String, Value>, c: &ReasoningControl) {
        reconcile_reasoning_passthrough(body, c);
    }
}

/// Render the normalised reasoning triple (§5.3) the way a llama-server takes
/// it: `reasoning_effort`, the budget, and the template's own
/// `chat_template_kwargs.enable_thinking`.
///
/// **The budget goes under both names** (I3). Official builds read
/// `reasoning_budget_tokens` first and `thinking_budget_tokens` after it
/// (`server-common.cpp:1390-1391` at 0c6a6a7); ik_llama.cpp reads only
/// `thinking_budget_tokens` (llama egress design §6), so a budget sent under
/// the newer name alone never reached it. Both carry the one resolved number.
///
/// OpenRouter's `reasoning` object rides along untouched by this step (the
/// shared builder reconciles it after the passthrough loop), but unlike on a
/// generic OpenAI-protocol route it does not silence the scalars: a
/// llama-server does not understand that object at all, so the scalar is the
/// only thing that can carry the control.
///
/// Called **before** the passthrough loop so that these keys, which the
/// gateway resolved from every tier, win over a stale copy riding along in
/// `passthrough` (that loop only fills keys the body does not already have).
fn apply_reasoning(body: &mut Map<String, Value>, ir: &ChatRequest, c: &ReasoningControl) {
    // `enable_thinking` is a *template* kwarg, so the client's own kwargs
    // object has to survive: this is an object-level deep merge, not the
    // whole-key passthrough rule that would discard it (§5.3).
    let mut kwargs: Map<String, Value> = ir
        .passthrough
        .get("chat_template_kwargs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut thinking: Option<bool> = None;

    if c.enabled == Some(false) {
        // `reasoning_effort: "none"` is a *newer master* special case — on
        // the build lmgw runs it is not one, and Qwen3.8's template raises on
        // a level it does not know. Verified against that build:
        // `chat_template_kwargs.enable_thinking` is what works, in both
        // directions, including on a row started with `--reasoning off`. So
        // the kwarg is the control here and no level is sent at all — a stray
        // `reasoning_effort` from passthrough is stripped by the reconcile,
        // since the template must not see a level while thinking is off.
        thinking = Some(false);
    } else {
        if let Some(e) = &c.effort {
            body.insert("reasoning_effort".into(), json!(e));
            // A row started with `--reasoning off` needs the template switched
            // on as well, or the level it was given has nothing to act on.
            thinking = Some(true);
        } else if c.enabled == Some(true) {
            thinking = Some(true);
        }
        if let Some(b) = c.budget_tokens {
            body.insert("reasoning_budget_tokens".into(), json!(b));
            body.insert("thinking_budget_tokens".into(), json!(b));
        }
    }

    if let Some(t) = thinking {
        kwargs.insert("enable_thinking".into(), json!(t));
    }
    if !kwargs.is_empty() {
        body.insert("chat_template_kwargs".into(), Value::Object(kwargs));
    }
}

/// Rewrite the client's own reasoning keys that rode through passthrough so
/// they cannot contradict the control lmgw resolved (§5.3): a level while
/// thinking is off, and a budget under the older name when lmgw resolved
/// none. OpenRouter's `reasoning` object is reconciled by the shared builder
/// ([`crate::egress::openai_wire::build_chat_body`]) right after this.
///
/// **A client's own `thinking_budget_tokens`** (the ingress reads it as the
/// budget, and leaves it in passthrough):
/// - a resolved budget: the key carries it, whatever the client wrote,
///   because [`apply_reasoning`] set it before the passthrough loop, which
///   only fills absent keys;
/// - a control without a budget (thinking off, or on with no number): it is
///   removed, so the server is never told a number lmgw did not resolve;
/// - no control at all: this step does not run, and the key goes verbatim.
///
/// Today's rule, which only ever touched this one name. A client's own
/// `reasoning_budget_tokens` is still left alone when no budget is resolved,
/// as before I3.
fn reconcile_reasoning_passthrough(body: &mut Map<String, Value>, c: &ReasoningControl) {
    // Thinking is off by template kwarg alone; a level that rode in through
    // passthrough would be handed to a template that must not see one (and,
    // on Qwen3.8, would raise).
    if c.enabled == Some(false) {
        body.remove("reasoning_effort");
    }
    if c.budget_tokens.is_none() {
        body.remove("thinking_budget_tokens");
    }
}
