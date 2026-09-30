//! A chat thread's reasoning overrides: the three values a client sends as
//! `x-lmgw-reasoning` (on/off), `x-lmgw-reasoning-effort` and
//! `x-lmgw-reasoning-budget`, kept with the thread so a conversation can try
//! them without the alias being edited. The fields in the thread settings,
//! the checks a save makes before the round trip, and the header's one-line
//! summary of what is overridden.

use leptos::prelude::*;
use serde_json::{json, Value};

use super::chat::ChatThread;
use crate::catalog::{use_model_catalog, ReasoningFacts};
use crate::widgets::Select;

/// The overrides as a save sends them, or why they cannot be — the checks
/// the server makes (a budget is a whole number of tokens, and on/off must
/// agree with the effort and the budget), made before the round trip.
pub(super) fn reasoning_patch(think: &str, effort: &str, budget: &str) -> Result<Value, String> {
    let enabled = match think {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    };
    let effort = Some(effort.trim()).filter(|e| !e.is_empty());
    let budget: Option<i64> = match budget.trim() {
        "" => None,
        b => match b.parse::<i64>() {
            Ok(n) if n >= 0 => Some(n),
            _ => return Err(format!("budget: '{b}' is not a whole number of tokens")),
        },
    };
    let effort_none = effort.is_some_and(|e| e.eq_ignore_ascii_case("none"));
    match enabled {
        Some(true) if effort_none => {
            return Err("thinking on contradicts an effort of 'none'".into())
        }
        Some(true) if budget == Some(0) => {
            return Err("thinking on contradicts a budget of 0".into())
        }
        Some(false) if effort.is_some() && !effort_none => {
            return Err("thinking off contradicts an effort — clear the effort".into())
        }
        Some(false) if budget.is_some_and(|b| b > 0) => {
            return Err("thinking off contradicts a budget — clear the budget".into())
        }
        _ => {}
    }
    Ok(json!({
        "reasoning_enabled": enabled,
        "reasoning_effort": effort,
        "reasoning_budget": budget,
    }))
}

/// The thread's overrides as one short line for the conversation's header,
/// `None` when it sets none.
pub(super) fn reasoning_badge(t: &ChatThread) -> Option<String> {
    let mut parts = Vec::new();
    match t.reasoning_enabled {
        Some(true) => parts.push("thinking on".to_string()),
        Some(false) => parts.push("thinking off".to_string()),
        None => {}
    }
    if let Some(e) = &t.reasoning_effort {
        parts.push(format!("effort {e}"));
    }
    if let Some(b) = t.reasoning_budget {
        parts.push(format!("budget {b}"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What the catalog says about a model's reasoning, as one line under the
/// fields — the default they would replace.
fn facts_line(f: Option<&ReasoningFacts>) -> String {
    let Some(f) = f else {
        return "The catalog states nothing about this model's reasoning; overrides are sent as \
                set."
            .into();
    };
    let default_state = match f.enabled {
        Some(true) => ", on by default",
        Some(false) => ", off by default",
        None => "",
    };
    let mut line = match f.kind.as_str() {
        "fixed" => format!(
            "This model's reasoning is fixed{}: nothing to change per request.",
            match f.enabled {
                Some(true) => " on",
                Some(false) => " off",
                None => "",
            }
        ),
        "levels" => {
            let mut l = format!("Effort levels: {}", f.levels.join(", "));
            if let Some(d) = &f.default {
                l.push_str(&format!("; default {d}"));
            }
            l.push_str(default_state);
            l.push('.');
            l
        }
        _ => format!("Thinking switches on or off per request{default_state}."),
    };
    if f.can_disable == Some(false) {
        line.push_str(" It cannot be switched off.");
    }
    if let Some(b) = f.budget_tokens {
        line.push_str(&format!(" Default budget {b} tokens."));
    }
    line
}

/// The three fields, the model's own reasoning facts under them, and why the
/// values as typed cannot be saved (`error`, which also blocks Save).
///
/// Effort is a pick from the model's levels when the catalog states them,
/// free text otherwise — lmgw checks no vocabulary, so a level the model
/// does not know is the model's own refusal, with its message.
#[component]
pub(super) fn ReasoningFields(
    /// The thread's model, whose catalog facts the fields describe.
    #[prop(into)]
    model: Signal<String>,
    think: RwSignal<String>,
    effort: RwSignal<String>,
    budget: RwSignal<String>,
    error: Memo<Option<String>>,
) -> impl IntoView {
    let catalog = use_model_catalog();
    let facts = Memo::new(move |_| {
        let model = model.get();
        catalog.entries.with(|es| {
            es.iter()
                .find(|e| e.id == model)
                .and_then(|e| e.reasoning_facts.clone())
        })
    });
    let think_opts = Signal::derive(|| {
        vec![
            (String::new(), "model default".to_string()),
            ("on".to_string(), "on".to_string()),
            ("off".to_string(), "off".to_string()),
        ]
    });
    let levels = Memo::new(move |_| {
        facts.with(|f| f.as_ref().map(|f| f.levels.clone()).unwrap_or_default())
    });
    let effort_opts = Signal::derive(move || {
        let default = facts
            .with(|f| f.as_ref().and_then(|f| f.default.clone()))
            .map(|d| format!("model default · {d}"))
            .unwrap_or_else(|| "model default".to_string());
        std::iter::once((String::new(), default))
            .chain(levels.get().into_iter().map(|l| (l.clone(), l)))
            .collect::<Vec<_>>()
    });
    let budget_ph = move || {
        facts
            .with(|f| f.as_ref().and_then(|f| f.budget_tokens))
            .map(|b| format!("model default · {b}"))
            .unwrap_or_else(|| "model default".to_string())
    };
    view! {
        <div class="field">
            <label>"Reasoning"</label>
            <div class="field-grid" style="--field-min:90px">
                <div class="field">
                    <label title="x-lmgw-reasoning">"Thinking"</label>
                    <Select value=think options=think_opts/>
                </div>
                <div class="field">
                    <label title="x-lmgw-reasoning-effort">"Effort"</label>
                    {move || {
                        if levels.with(Vec::is_empty) {
                            view! {
                                <input
                                    class="input mono"
                                    placeholder="model default"
                                    prop:value=move || effort.get()
                                    on:input=move |ev| effort.set(event_target_value(&ev))
                                />
                            }
                                .into_any()
                        } else {
                            view! { <Select value=effort options=effort_opts/> }.into_any()
                        }
                    }}
                </div>
                <div class="field">
                    <label title="x-lmgw-reasoning-budget">"Budget"</label>
                    <input
                        class="input mono"
                        inputmode="numeric"
                        placeholder=budget_ph
                        prop:value=move || budget.get()
                        on:input=move |ev| budget.set(event_target_value(&ev))
                    />
                </div>
            </div>
            {move || error.get().map(|e| view! { <div class="notice warn">{e}</div> })}
            <div class="field-hint">
                {move || facts.with(|f| facts_line(f.as_ref()))}
                " Sent like the x-lmgw-reasoning, -effort and -budget headers: over the \
                 model's own defaults, for this thread only."
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_are_checked_like_the_headers() {
        assert_eq!(
            reasoning_patch("", " ", "").unwrap(),
            json!({ "reasoning_enabled": null, "reasoning_effort": null, "reasoning_budget": null })
        );
        assert_eq!(
            reasoning_patch("on", " high ", "2048").unwrap(),
            json!({ "reasoning_enabled": true, "reasoning_effort": "high", "reasoning_budget": 2048 })
        );
        assert!(reasoning_patch("off", "none", "0").is_ok());
        for (think, effort, budget) in [
            ("on", "none", ""),
            ("on", "", "0"),
            ("off", "low", ""),
            ("off", "", "100"),
            ("", "", "-1"),
            ("", "", "lots"),
        ] {
            assert!(
                reasoning_patch(think, effort, budget).is_err(),
                "{think:?} {effort:?} {budget:?}"
            );
        }
    }

    #[test]
    fn the_header_names_each_override_once() {
        let t = ChatThread {
            reasoning_enabled: Some(false),
            ..Default::default()
        };
        assert_eq!(reasoning_badge(&t).as_deref(), Some("thinking off"));
        let t = ChatThread {
            reasoning_effort: Some("high".into()),
            reasoning_budget: Some(64),
            ..Default::default()
        };
        assert_eq!(
            reasoning_badge(&t).as_deref(),
            Some("effort high · budget 64")
        );
        assert_eq!(reasoning_badge(&ChatThread::default()), None);
    }

    #[test]
    fn the_facts_line_says_what_the_default_is() {
        let levels = ReasoningFacts {
            kind: "levels".into(),
            levels: vec!["low".into(), "medium".into(), "high".into()],
            default: Some("medium".into()),
            can_disable: Some(false),
            ..Default::default()
        };
        assert_eq!(
            facts_line(Some(&levels)),
            "Effort levels: low, medium, high; default medium. It cannot be switched off."
        );
        let toggle = ReasoningFacts {
            kind: "toggle".into(),
            enabled: Some(true),
            ..Default::default()
        };
        assert_eq!(
            facts_line(Some(&toggle)),
            "Thinking switches on or off per request, on by default."
        );
        assert!(facts_line(None).starts_with("The catalog states nothing"));
    }
}
