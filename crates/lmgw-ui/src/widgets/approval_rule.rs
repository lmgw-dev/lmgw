//! A tool source's `require_approval` (client-apps design §6.1) as the tool
//! picker edits it: never, always, or a list of the tools that ask first.
//!
//! The store keeps OpenAI's shapes: `"never"`, `"always"`, or
//! `{"always": {"tool_names": […]}, "never": {"tool_names": […]}}`. The picker
//! writes exactly three of them. A value it cannot show as one of the three
//! (a `never` list, which gates every other tool; anything a client wrote that
//! is not a name list) is [`Rule::Custom`]: it is kept as read, and replaced
//! only when the owner picks a mode.

use serde_json::{json, Value};

/// What a source's approval rule is, to the picker.
#[derive(Debug, Clone, PartialEq)]
pub enum Rule {
    /// Nothing waits: the API's default, also when the field is absent.
    Never,
    /// Every call of the source waits.
    Always,
    /// The listed tools wait; the rest run.
    Tools(Vec<String>),
    /// A rule written elsewhere that the three modes cannot show.
    Custom(Value),
}

/// The part of a tool's name after its source's `<prefix>__`.
fn own_name(name: &str) -> &str {
    name.split_once("__").map_or(name, |(_, rest)| rest)
}

fn names(v: &Value) -> Option<Vec<String>> {
    v.as_array()?
        .iter()
        .map(|n| n.as_str().map(str::to_string))
        .collect()
}

impl Rule {
    /// Read the stored field.
    pub fn read(v: Option<&Value>) -> Self {
        match v {
            None | Some(Value::Null) => Self::Never,
            Some(Value::String(s)) if s == "never" => Self::Never,
            Some(Value::String(s)) if s == "always" => Self::Always,
            Some(Value::Object(o)) => {
                let listed = |key: &str| -> Option<Vec<String>> {
                    match o.get(key) {
                        None | Some(Value::Null) => Some(Vec::new()),
                        Some(Value::Object(f)) if f.keys().all(|k| k == "tool_names") => {
                            f.get("tool_names").map_or(Some(Vec::new()), names)
                        }
                        _ => None,
                    }
                };
                match (listed("always"), listed("never")) {
                    (Some(always), Some(never))
                        if never.is_empty() && o.keys().all(|k| k == "always" || k == "never") =>
                    {
                        Self::Tools(always)
                    }
                    _ => Self::Custom(v.cloned().unwrap_or(Value::Null)),
                }
            }
            Some(other) => Self::Custom(other.clone()),
        }
    }

    /// The field to store: absent for `Never`.
    pub fn write(&self) -> Option<Value> {
        match self {
            Self::Never => None,
            Self::Always => Some(json!("always")),
            Self::Tools(list) => Some(json!({ "always": { "tool_names": list } })),
            Self::Custom(v) => Some(v.clone()),
        }
    }

    /// Does `tool` (its exposed name) ask first? Either spelling of the name
    /// matches, as the gateway's rule does.
    pub fn asks(&self, tool: &str) -> bool {
        match self {
            Self::Always => true,
            Self::Tools(list) => list.iter().any(|n| n == tool || n == own_name(tool)),
            Self::Never | Self::Custom(_) => false,
        }
    }

    /// Is the per-tool list the mode in force?
    pub fn is_tools(&self) -> bool {
        matches!(self, Self::Tools(_))
    }

    /// Tick or untick one tool of a per-tool list (a no-op in any other mode).
    pub fn set_ask(&mut self, tool: &str, on: bool) {
        let Self::Tools(list) = self else { return };
        list.retain(|n| n != tool && n != own_name(tool));
        if on {
            list.push(tool.to_string());
        }
    }

    /// The per-tool mode, keeping the list that was there.
    pub fn into_tools(self) -> Self {
        match self {
            tools @ Self::Tools(_) => tools,
            _ => Self::Tools(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(v: Value) -> Rule {
        Rule::read(Some(&v))
    }

    #[test]
    fn the_three_modes_read_and_write_back() {
        assert_eq!(Rule::read(None), Rule::Never);
        assert_eq!(rule(json!("never")), Rule::Never);
        assert_eq!(Rule::Never.write(), None);
        assert_eq!(rule(json!("always")), Rule::Always);
        assert_eq!(Rule::Always.write(), Some(json!("always")));
        let tools = rule(json!({"always": {"tool_names": ["a", "b"]}}));
        assert_eq!(tools, Rule::Tools(vec!["a".into(), "b".into()]));
        assert_eq!(
            tools.write(),
            Some(json!({"always": {"tool_names": ["a", "b"]}}))
        );
    }

    #[test]
    fn what_the_modes_cannot_show_is_kept_as_read() {
        for v in [
            json!({"never": {"tool_names": ["x"]}}),
            json!({"always": {"tool_names": ["a"]}, "never": {"tool_names": ["b"]}}),
            json!({"always": {"read_only": true}}),
            json!("sometimes"),
            json!(3),
        ] {
            let r = rule(v.clone());
            assert_eq!(r, Rule::Custom(v.clone()), "{v}");
            assert_eq!(r.write(), Some(v));
        }
    }

    #[test]
    fn a_tool_asks_by_either_spelling_and_ticks_replace_both() {
        let mut r = rule(json!({"always": {"tool_names": ["send"]}}));
        assert!(r.asks("mail__send"));
        assert!(r.asks("send"));
        assert!(!r.asks("mail__list"));
        r.set_ask("mail__send", false);
        assert_eq!(r, Rule::Tools(vec![]));
        r.set_ask("mail__list", true);
        assert!(r.asks("mail__list"));
        assert!(Rule::Always.asks("anything"));
        assert!(!Rule::Never.asks("anything"));
        // Other modes ignore a tick.
        let mut never = Rule::Never;
        never.set_ask("x", true);
        assert_eq!(never, Rule::Never);
    }

    #[test]
    fn picking_the_list_mode_keeps_an_existing_list() {
        let r = Rule::Tools(vec!["a".into()]);
        assert_eq!(r.clone().into_tools(), r);
        assert_eq!(Rule::Always.into_tools(), Rule::Tools(vec![]));
    }
}
