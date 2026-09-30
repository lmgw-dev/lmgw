//! `{{path}}` substitution, and nothing else (agent-catalog design §2.3).
//!
//! No conditionals, no filters, no arithmetic. The escape hatch for anything a
//! manifest cannot express is an MCP server, where the Podman isolation already
//! is — lmgw grows no scripting language (§1, principle 1).
//!
//! Two rendering modes, decided by the shape of the string:
//!
//! - a string that **is exactly one placeholder** takes the referenced value
//!   with its JSON type, so `"maxResults": "{{config.limit}}"` becomes the
//!   integer `50` and `"rows": "{{rows}}"` becomes the array;
//! - a string with placeholders among other text renders each one into text.
//!
//! Lives next to [`super::manifest`], which owns the compile-time half: a
//! `config.<field>` that names nothing in the config schema is a validation
//! error at save or import ([`validate`]), never a runtime surprise.

use serde_json::{Map, Value};

/// The roots a placeholder may start with (§2.3).
///
/// `Config` and `Agent`/`Run` are always resolvable; the rest depend on where
/// in the run the template sits, which is why [`validate`] takes the permitted
/// set rather than assuming all six.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    /// Stored values over schema defaults.
    Config,
    /// One element of the source result.
    Item,
    /// The item step's fetch result.
    Fetched,
    /// The reviewed, checked rows handed to apply.
    Rows,
    /// `id`, `name`.
    Agent,
    /// `id`.
    Run,
}

impl Root {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Item => "item",
            Self::Fetched => "fetched",
            Self::Rows => "rows",
            Self::Agent => "agent",
            Self::Run => "run",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "config" => Self::Config,
            "item" => Self::Item,
            "fetched" => Self::Fetched,
            "rows" => Self::Rows,
            "agent" => Self::Agent,
            "run" => Self::Run,
            _ => return None,
        })
    }

    /// Every root there is, for an error message that lists the alternatives.
    pub const ALL: [Root; 6] = [
        Root::Config,
        Root::Item,
        Root::Fetched,
        Root::Rows,
        Root::Agent,
        Root::Run,
    ];
}

/// The values a render resolves against. Absent roots stay [`Value::Null`],
/// which renders as empty — `item.*` and `fetched.*` cannot be checked ahead
/// (tool payloads vary), so a missing path is data, not a crash (§2.3).
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub config: Value,
    pub item: Value,
    pub fetched: Value,
    pub rows: Value,
    pub agent: Value,
    pub run: Value,
}

impl Ctx {
    /// `agent` and `run` in the shape §2.3 gives them, so callers do not each
    /// hand-build the same two objects.
    pub fn with_identity(mut self, agent_id: &str, agent_name: &str, run_id: Option<i64>) -> Self {
        self.agent = serde_json::json!({ "id": agent_id, "name": agent_name });
        self.run = match run_id {
            Some(id) => serde_json::json!({ "id": id }),
            None => Value::Null,
        };
        self
    }

    fn root(&self, name: &str) -> Option<&Value> {
        Some(match Root::parse(name)? {
            Root::Config => &self.config,
            Root::Item => &self.item,
            Root::Fetched => &self.fetched,
            Root::Rows => &self.rows,
            Root::Agent => &self.agent,
            Root::Run => &self.run,
        })
    }

    /// Resolve a dotted path. `None` means "no such path", which the two render
    /// modes turn into `null` and `""` respectively.
    pub fn lookup(&self, path: &str) -> Option<&Value> {
        let mut parts = path.split('.');
        let mut cur = self.root(parts.next()?)?;
        for seg in parts {
            cur = match cur {
                Value::Object(map) => map.get(seg)?,
                Value::Array(items) => items.get(seg.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(cur)
    }
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// One `{{…}}` occurrence: the trimmed path and its byte span in the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Placeholder {
    pub path: String,
    pub start: usize,
    pub end: usize,
}

/// Every placeholder in `s`, in order. An unterminated `{{` is not a
/// placeholder — it renders verbatim, the same as any other text.
pub fn placeholders(s: &str) -> Vec<Placeholder> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(rel) = s[i + 2..].find("}}") {
                let inner_end = i + 2 + rel;
                out.push(Placeholder {
                    path: s[i + 2..inner_end].trim().to_string(),
                    start: i,
                    end: inner_end + 2,
                });
                i = inner_end + 2;
                continue;
            }
            // No closing `}}` anywhere after this point: nothing else can be a
            // placeholder either.
            break;
        }
        i += 1;
    }
    out
}

/// Whether `s` is *exactly* one placeholder and nothing else — the whole-value
/// typing rule of §2.3. Surrounding whitespace counts as other text: a template
/// author who wrote `" {{config.limit}}"` asked for a string.
pub fn whole_value_path(s: &str) -> Option<&str> {
    let inner = s.strip_prefix("{{")?.strip_suffix("}}")?;
    // A second `}}` inside would mean this is two placeholders, not one.
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    Some(inner.trim())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// One value rendered into a *string* (§2.3): strings verbatim, numbers and
/// booleans as JSON text, arrays of scalars comma-joined, objects (and arrays
/// holding them) as compact JSON. `null` and absent both render empty.
pub fn value_to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(_) | Value::Number(_) => v.to_string(),
        Value::Array(items) => {
            if items.iter().all(|i| !i.is_array() && !i.is_object()) {
                items
                    .iter()
                    .map(value_to_text)
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                v.to_string()
            }
        }
        Value::Object(_) => v.to_string(),
    }
}

/// Render a template string in text mode. Every placeholder becomes
/// [`value_to_text`] of what it resolves to; an unresolvable path becomes the
/// empty string.
pub fn render_text(template: &str, ctx: &Ctx) -> String {
    let marks = placeholders(template);
    if marks.is_empty() {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let mut cursor = 0;
    for m in marks {
        out.push_str(&template[cursor..m.start]);
        out.push_str(&value_to_text(ctx.lookup(&m.path).unwrap_or(&Value::Null)));
        cursor = m.end;
    }
    out.push_str(&template[cursor..]);
    out
}

/// Render a JSON value: every string inside is rendered, with the whole-value
/// rule applied per string. Objects and arrays are walked, so a step's `args`
/// keeps its shape and only its leaves change.
pub fn render_value(v: &Value, ctx: &Ctx) -> Value {
    match v {
        Value::String(s) => match whole_value_path(s) {
            Some(path) => ctx.lookup(path).cloned().unwrap_or(Value::Null),
            None => Value::String(render_text(s, ctx)),
        },
        Value::Array(items) => Value::Array(items.iter().map(|i| render_value(i, ctx)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, val)| (k.clone(), render_value(val, ctx)))
                .collect::<Map<String, Value>>(),
        ),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// What a `config.<field>` reference is checked against: the field names the
/// config schema declares. Kept as a borrowed slice so the caller (the manifest
/// validator) owns the schema.
pub type ConfigFields<'a> = &'a [String];

fn list_roots(allowed: &[Root]) -> String {
    allowed
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Check every placeholder in `template`.
///
/// `where_` names the manifest position in the error, because "unknown root
/// 'items'" with no location is unactionable in a manifest with eight templates
/// in it. Errors are collected rather than short-circuited: a paste with three
/// typos should report three, not one per save.
pub fn validate(
    template: &str,
    where_: &str,
    allowed: &[Root],
    config_fields: ConfigFields<'_>,
    errors: &mut Vec<String>,
) {
    for m in placeholders(template) {
        if m.path.is_empty() {
            errors.push(format!("{where_}: empty placeholder '{{{{}}}}'"));
            continue;
        }
        let mut parts = m.path.split('.');
        let root_name = parts.next().unwrap_or_default();
        let Some(root) = Root::parse(root_name) else {
            errors.push(format!(
                "{where_}: unknown template root '{root_name}' in '{{{{{}}}}}' (roots: {})",
                m.path,
                list_roots(&Root::ALL)
            ));
            continue;
        };
        if !allowed.contains(&root) {
            errors.push(format!(
                "{where_}: '{{{{{}}}}}' is not available here (available: {})",
                m.path,
                list_roots(allowed)
            ));
            continue;
        }
        let rest: Vec<&str> = parts.collect();
        match root {
            // The only root whose shape is known ahead of the run.
            Root::Config => {
                let Some(field) = rest.first() else {
                    errors.push(format!(
                        "{where_}: '{{{{config}}}}' names no field — write config.<field>"
                    ));
                    continue;
                };
                if !config_fields.iter().any(|f| f == field) {
                    errors.push(format!(
                        "{where_}: '{{{{{}}}}}' names no config field ({})",
                        m.path,
                        if config_fields.is_empty() {
                            "this agent declares no config schema".to_string()
                        } else {
                            format!("fields: {}", config_fields.join(", "))
                        }
                    ));
                }
            }
            Root::Agent => match rest.first() {
                Some(&"id") | Some(&"name") => {}
                _ => errors.push(format!(
                    "{where_}: '{{{{{}}}}}' — agent carries id and name only",
                    m.path
                )),
            },
            Root::Run => match rest.first() {
                Some(&"id") => {}
                _ => errors.push(format!(
                    "{where_}: '{{{{{}}}}}' — run carries id only",
                    m.path
                )),
            },
            // Tool payloads vary, so these cannot be checked ahead (§2.3).
            Root::Item | Root::Fetched | Root::Rows => {}
        }
    }
}

/// [`validate`] over every string inside a JSON value (a step's `args`).
pub fn validate_value(
    v: &Value,
    where_: &str,
    allowed: &[Root],
    config_fields: ConfigFields<'_>,
    errors: &mut Vec<String>,
) {
    match v {
        Value::String(s) => validate(s, where_, allowed, config_fields, errors),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                validate_value(
                    item,
                    &format!("{where_}[{i}]"),
                    allowed,
                    config_fields,
                    errors,
                );
            }
        }
        Value::Object(map) => {
            for (k, val) in map {
                validate_value(
                    val,
                    &format!("{where_}.{k}"),
                    allowed,
                    config_fields,
                    errors,
                );
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Ctx {
        Ctx {
            config: json!({
                "limit": 50,
                "categories": ["Work", "Finance"],
                "label_prefix": "lmgw",
                "dry": true,
                "ratio": 0.5,
                "nested": { "a": 1 },
            }),
            item: json!({ "id": "m1" }),
            fetched: json!({ "from": "a@b.test", "body": null }),
            rows: json!([{ "id": "m1", "category": "Work" }]),
            ..Default::default()
        }
        .with_identity("mail-labeler", "Mail labeler", Some(7))
    }

    // ---- whole-value typing (§2.3) ----

    #[test]
    fn a_string_that_is_one_placeholder_takes_the_value_with_its_type() {
        let c = ctx();
        assert_eq!(render_value(&json!("{{config.limit}}"), &c), json!(50));
        assert_eq!(
            render_value(&json!("{{rows}}"), &c),
            json!([{ "id": "m1", "category": "Work" }])
        );
        assert_eq!(render_value(&json!("{{config.dry}}"), &c), json!(true));
        assert_eq!(
            render_value(&json!("{{config.categories}}"), &c),
            json!(["Work", "Finance"])
        );
        // Inner whitespace is fine; surrounding text is not — that is a string.
        assert_eq!(render_value(&json!("{{ config.limit }}"), &c), json!(50));
        assert_eq!(render_value(&json!(" {{config.limit}}"), &c), json!(" 50"));
    }

    #[test]
    fn an_unresolvable_whole_value_is_null_and_keeps_its_key() {
        let c = ctx();
        let out = render_value(&json!({ "q": "{{fetched.nope}}" }), &c);
        assert_eq!(out, json!({ "q": null }));
    }

    #[test]
    fn nested_args_keep_their_shape() {
        let c = ctx();
        let out = render_value(
            &json!({ "query": "is:unread {{config.label_prefix}}", "page": { "max": "{{config.limit}}" } }),
            &c,
        );
        assert_eq!(
            out,
            json!({ "query": "is:unread lmgw", "page": { "max": 50 } })
        );
    }

    // ---- mixed-string rendering of each JSON type ----

    #[test]
    fn mixed_strings_render_every_json_type() {
        let c = ctx();
        assert_eq!(render_text("n={{config.limit}}", &c), "n=50");
        assert_eq!(render_text("r={{config.ratio}}", &c), "r=0.5");
        assert_eq!(render_text("d={{config.dry}}", &c), "d=true");
        assert_eq!(render_text("s={{config.label_prefix}}", &c), "s=lmgw");
        // arrays of scalars comma-joined, objects as compact JSON
        assert_eq!(
            render_text("cats: {{config.categories}}.", &c),
            "cats: Work, Finance."
        );
        assert_eq!(render_text("{{config.nested}}", &c), r#"{"a":1}"#);
        assert_eq!(
            render_text("{{rows}}", &c),
            r#"[{"category":"Work","id":"m1"}]"#
        );
        // several in one string, and text between them
        assert_eq!(
            render_text(
                "{{item.id}} -> {{config.label_prefix}}/{{config.limit}}",
                &c
            ),
            "m1 -> lmgw/50"
        );
    }

    #[test]
    fn an_absent_path_renders_empty_and_json_null_does_too() {
        let c = ctx();
        assert_eq!(render_text("[{{fetched.subject}}]", &c), "[]");
        assert_eq!(render_text("[{{item.nope.deeper}}]", &c), "[]");
        assert_eq!(render_text("[{{fetched.body}}]", &c), "[]");
        assert_eq!(render_text("[{{rows.0.category}}]", &c), "[Work]");
    }

    #[test]
    fn identity_roots_resolve() {
        let c = ctx();
        assert_eq!(
            render_text("{{agent.name}} ({{agent.id}}) run {{run.id}}", &c),
            "Mail labeler (mail-labeler) run 7"
        );
    }

    #[test]
    fn an_unterminated_placeholder_is_plain_text() {
        let c = ctx();
        assert_eq!(render_text("{{config.limit", &c), "{{config.limit");
        assert_eq!(render_text("a {{ b", &c), "a {{ b");
        assert!(placeholders("{{config.limit").is_empty());
    }

    // ---- validation ----

    fn fields() -> Vec<String> {
        vec!["limit".to_string(), "categories".to_string()]
    }

    #[test]
    fn an_unknown_config_field_is_a_validation_error() {
        let mut errs = Vec::new();
        validate(
            "{{config.nope}}",
            "item.user",
            &[Root::Config],
            &fields(),
            &mut errs,
        );
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("item.user"), "{errs:?}");
        assert!(errs[0].contains("config.nope"), "{errs:?}");
        assert!(errs[0].contains("limit"), "{errs:?}");
    }

    #[test]
    fn an_unknown_root_names_the_alternatives() {
        let mut errs = Vec::new();
        validate(
            "{{items.id}}",
            "run.source.args.q",
            &[Root::Config, Root::Item],
            &fields(),
            &mut errs,
        );
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].contains("unknown template root 'items'"),
            "{errs:?}"
        );
        assert!(errs[0].contains("fetched"), "{errs:?}");
    }

    #[test]
    fn a_root_that_is_not_bound_here_says_so() {
        let mut errs = Vec::new();
        validate(
            "{{rows}}",
            "run.source",
            &[Root::Config, Root::Agent],
            &fields(),
            &mut errs,
        );
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("not available here"), "{errs:?}");
        assert!(errs[0].contains("config, agent"), "{errs:?}");
    }

    #[test]
    fn item_and_fetched_paths_are_never_refused_ahead_of_the_run() {
        let mut errs = Vec::new();
        validate(
            "{{item.whatever.deep}} {{fetched.x}}",
            "item.user",
            &[Root::Item, Root::Fetched],
            &fields(),
            &mut errs,
        );
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn identity_roots_are_checked_because_their_shape_is_fixed() {
        let mut errs = Vec::new();
        validate(
            "{{agent.version}} {{run.started}}",
            "run.apply.turn.prompt",
            &[Root::Agent, Root::Run],
            &fields(),
            &mut errs,
        );
        assert_eq!(errs.len(), 2, "{errs:?}");
    }

    #[test]
    fn validation_walks_into_step_args() {
        let mut errs = Vec::new();
        validate_value(
            &json!({ "q": "{{config.nope}}", "page": ["{{config.limit}}", "{{config.also_nope}}"] }),
            "run.source.args",
            &[Root::Config],
            &fields(),
            &mut errs,
        );
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("args.q")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("args.page[1]")), "{errs:?}");
    }
}
