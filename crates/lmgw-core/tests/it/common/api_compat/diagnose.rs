//! A validator's errors as the few leaves a reader needs.
//!
//! A provider's schema is mostly unions (`anyOf`/`oneOf` of message kinds,
//! content blocks, tools), and a union error lists every alternative's
//! failures: the one alternative the body meant, beside a dozen that fail on
//! their `type` tag. [`leaves`] follows the alternative the body meant (the
//! one whose tag matched, or that got furthest) down to the failures that are
//! really wrong, so a report names `/messages/1/content/0/input` and not the
//! whole message.

use jsonschema::error::ValidationErrorKind;
use jsonschema::ValidationError;
use serde_json::Value;

/// One failure, where it is in the body and what rule it broke.
#[derive(Debug, Clone, PartialEq)]
pub struct Leaf {
    /// The JSON pointer into the body.
    pub path: String,
    /// The schema keyword that failed (`type`, `required`,
    /// `additionalProperties`, `anyOf` when no alternative fits at all, …).
    pub keyword: String,
    /// The unknown keys of an `additionalProperties` failure.
    pub unexpected: Vec<String>,
    /// The property a `required` failure says is missing.
    pub missing: Option<String>,
    /// The validator's own sentence.
    pub message: String,
    /// The failing value itself.
    pub value: Value,
}

impl std::fmt::Display for Leaf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let at = if self.path.is_empty() {
            "/"
        } else {
            &self.path
        };
        write!(f, "{at}: {} ({})", self.message, self.keyword)
    }
}

/// The leaves of `instance`'s errors against `validator`.
pub fn leaves(validator: &jsonschema::Validator, instance: &Value) -> Vec<Leaf> {
    let mut out = Vec::new();
    for e in validator.iter_errors(instance) {
        push(&e, &mut out);
    }
    out.dedup();
    out
}

fn push(e: &ValidationError<'_>, out: &mut Vec<Leaf>) {
    let leaf = |keyword: &str, unexpected: Vec<String>, message: String| Leaf {
        path: e.instance_path().as_str().to_string(),
        keyword: keyword.to_string(),
        unexpected,
        missing: match e.kind() {
            ValidationErrorKind::Required { property } => {
                Some(property.as_str().unwrap_or_default().to_string())
            }
            _ => None,
        },
        message,
        value: e.instance().clone().into_owned(),
    };
    match e.kind() {
        ValidationErrorKind::AnyOf { context } | ValidationErrorKind::OneOfNotValid { context } => {
            match best(e.instance_path().as_str(), context) {
                Some(branch) => branch.iter().for_each(|b| push(b, out)),
                None => out.push(leaf(
                    e.kind().keyword(),
                    vec![],
                    format!("no alternative of the union fits {}", short(e.instance())),
                )),
            }
        }
        ValidationErrorKind::AdditionalProperties { unexpected } => out.push(leaf(
            "additionalProperties",
            unexpected.clone(),
            format!("unknown key(s) {unexpected:?}"),
        )),
        kind => out.push(leaf(kind.keyword(), vec![], e.to_string())),
    }
}

/// The alternative the body meant, or `None` when every alternative fails
/// already on what kind of value it is (its tag or its type).
fn best<'a>(
    at: &str,
    context: &'a [Vec<ValidationError<'static>>],
) -> Option<&'a Vec<ValidationError<'static>>> {
    let wrong_kind = |errors: &[ValidationError<'static>]| {
        errors.iter().any(|e| {
            let p = e.instance_path().as_str();
            let tag = p
                .strip_prefix(at)
                .is_some_and(|rest| matches!(rest, "/type" | "/role" | "/object"));
            let tagged = tag
                && matches!(
                    e.kind(),
                    ValidationErrorKind::Constant { .. } | ValidationErrorKind::Enum { .. }
                );
            let typed = p == at && matches!(e.kind(), ValidationErrorKind::Type { .. });
            tagged || typed
        })
    };
    let depth = |errors: &[ValidationError<'static>]| {
        errors
            .iter()
            .map(|e| e.instance_path().as_str().matches('/').count())
            .max()
            .unwrap_or(0)
    };
    context
        .iter()
        .filter(|b| !b.is_empty() && !wrong_kind(b))
        .max_by_key(|b| (depth(b), usize::MAX - b.len()))
}

/// A value, cut to a line.
pub fn short(v: &Value) -> String {
    let s = v.to_string();
    match s.char_indices().nth(160) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}
