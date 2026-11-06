//! The platform's reasoning vocabulary (openai egress design §7.1): one
//! scalar, `reasoning_effort`, and OpenRouter's `reasoning` object translated
//! into it rather than forwarded (P3).
//!
//! The Chat reference (checked 2026-10-24,
//! <https://developers.openai.com/api/reference/resources/chat.md>, "Create
//! chat completion") has `reasoning_effort` — "`none`, `minimal`, `low`,
//! `medium`, `high`, `xhigh`, and `max`", "not all reasoning models support
//! every value" — and no `reasoning` object: that object is an aggregator's
//! dialect, and OpenAI rejects a parameter it does not know. The ingress has
//! already read the object's `effort`, `enabled` and `max_tokens` into the
//! control (`ingress/openai.rs`, `parse_reasoning`), so the control is what
//! goes out, and the object does not.
//!
//! What the object says that `reasoning_effort` cannot: a bare `enabled:
//! true` and a `max_tokens` budget, which `x-lmgw-reasoning-ignored` names
//! (`proxy::reasoning_ignored`'s platform arm). Its `exclude` is not named
//! yet: `exclude: true` is what the platform's chat wire does anyway (it
//! returns no reasoning text), and `exclude: false` asks for text it never
//! returns.

use serde_json::{json, Map, Value};

use crate::egress::openai_wire::ReasoningStep;
use crate::ir::{ChatRequest, ReasoningControl};

/// The platform's [`ReasoningStep`]: the control as `reasoning_effort`
/// before the passthrough loop, whatever dialect the client spoke, so it
/// wins over a stale scalar riding in passthrough. Nothing after the loop:
/// [`drop_reasoning_object`] runs on the finished body.
pub(super) struct PlatformReasoning;

impl ReasoningStep for PlatformReasoning {
    fn apply(&self, body: &mut Map<String, Value>, _ir: &ChatRequest, c: &ReasoningControl) {
        if c.enabled == Some(false) {
            body.insert("reasoning_effort".into(), json!("none"));
        } else if let Some(e) = &c.effort {
            body.insert("reasoning_effort".into(), json!(e));
        }
    }

    fn reconcile(&self, _body: &mut Map<String, Value>, _c: &ReasoningControl) {}
}

/// Take OpenRouter's `reasoning` object out of a finished body: its contents
/// went into the control, which [`PlatformReasoning`] wrote. On the finished
/// body rather than in [`ReasoningStep::reconcile`], which runs only when the
/// control is set: an object that set nothing lmgw reads (only `exclude`)
/// must not go either. A `reasoning` that is not an object is a client's own
/// key, and goes as it came.
pub(super) fn drop_reasoning_object(body: &mut Value) {
    if let Some(o) = body.as_object_mut() {
        if o.get("reasoning").is_some_and(Value::is_object) {
            o.remove("reasoning");
        }
    }
}
