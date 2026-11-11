//! What of a request reaches an audit entry (§5.8): the row it names, a
//! settings save's changed fields, and never a secret's value.
//!
//! The redaction list is not written here: it is every property and
//! parameter name the API document flags `x-lmgw-secret` (the same flags
//! the API reference's "Copy as curl" replaces with placeholders, pinned by
//! `openapi_structure::secret_inputs_per_operation`). A field flagged there
//! is redacted here by its name wherever it appears in an entry, a settings
//! field included, and `audit::redaction` fails for a request-body or
//! settings field whose name reads as a credential but carries no flag.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::{Map, Value};

/// What a redacted value reads in an entry.
pub const CHANGED: &str = "changed";

/// The arguments an op or tool names its row with: copied into an entry's
/// target when they are a plain value, every other argument left out (a
/// prompt, a document, a file's content). Words that name, never content.
pub const IDENTITY: &[&str] = &[
    "id",
    "name",
    "login",
    "display_name",
    "account",
    "account_id",
    "key_id",
    "kind",
    "model",
    "model_id",
    "alias",
    "upstream",
    "upstream_id",
    "agent_id",
    "server",
    "repo",
    "file",
    "target",
    "action",
    "enabled",
];

/// Every name the API document flags as a secret, anywhere in it.
pub fn secret_names() -> &'static BTreeSet<String> {
    static NAMES: OnceLock<BTreeSet<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut out = BTreeSet::new();
        collect(crate::openapi::admin_doc(), &mut out);
        out
    })
}

fn flagged(v: &Value) -> bool {
    v.get(lmgw_api_types::openapi_ext::SECRET) == Some(&Value::Bool(true))
}

fn collect(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            if let Some(Value::Object(props)) = map.get("properties") {
                for (name, prop) in props {
                    if flagged(prop) {
                        out.insert(name.clone());
                    }
                }
            }
            // A parameter object (`{"name", "in", ...}`).
            if let (Some(Value::String(name)), Some(_)) = (map.get("name"), map.get("in")) {
                if flagged(v) {
                    out.insert(name.clone());
                }
            }
            for v in map.values() {
                collect(v, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
        _ => {}
    }
}

/// Whether `name` is on the redaction list.
pub fn is_secret(name: &str) -> bool {
    secret_names().contains(name)
}

/// `value` with every member whose name is on the list replaced by
/// [`CHANGED`], at any depth.
pub fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_secret(k) {
                    *v = Value::String(CHANGED.into());
                } else {
                    redact(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// The [`IDENTITY`] arguments of `args` that are a plain value, redacted.
pub fn identity(args: &Map<String, Value>) -> Map<String, Value> {
    let mut out: Map<String, Value> = args
        .iter()
        .filter(|(k, v)| {
            IDENTITY.contains(&k.as_str())
                && matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut wrapped = Value::Object(std::mem::take(&mut out));
    redact(&mut wrapped);
    match wrapped {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// The ops and tools whose arguments are a settings patch: their entry
/// names the fields the request itself asked for ([`patch_fields`]), not
/// what changed between the settings before and after it, which another
/// request may have moved meanwhile (D85).
pub const PATCH_OPS: &[&str] = &["settings_set", "settings_set_full", "lmgw__settings_set"];

/// Settings whose value is prose or a command line, which a settings field
/// in an entry names as changed with its length and never quotes (D92): a
/// prompt or an instruction may hold anything someone wrote, a container's
/// extra arguments a token in a flag.
pub fn free_text(name: &str) -> bool {
    name.ends_with("prompt")
        || name.ends_with("instructions")
        || name.ends_with("speech_style")
        || name == "extra_run_args"
}

/// What a free-text setting reads in an entry: changed, with its length.
fn described(v: &Value) -> Value {
    Value::String(match v {
        Value::String(s) => format!("{CHANGED} ({} characters)", s.chars().count()),
        Value::Array(items) => format!("{CHANGED} ({} arguments)", items.len()),
        Value::Null => format!("{CHANGED} (none)"),
        _ => CHANGED.to_string(),
    })
}

/// `text` without the user name and password a URL may carry before its
/// host (`https://user:pass@host/` → `https://host/`, D92); any other text
/// as it is.
pub fn strip_userinfo(text: &str) -> String {
    let Some(scheme_end) = text.find("://") else {
        return text.to_string();
    };
    let rest = &text[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{}{}", &text[..scheme_end + 3], &rest[at + 1..]),
        None => text.to_string(),
    }
}

/// `v` with every URL's user name and password removed, at any depth.
fn strip_urls(v: &mut Value) {
    match v {
        Value::String(s) if s.contains("://") => *s = strip_userinfo(s),
        Value::Array(items) => items.iter_mut().for_each(strip_urls),
        Value::Object(map) => map.values_mut().for_each(strip_urls),
        _ => {}
    }
}

/// A settings field's value as an entry holds it: [`CHANGED`] under a name
/// on the list, its length for [`free_text`], and otherwise the value with
/// every secret inside it redacted and every URL's sign-in removed.
fn field_value(key: &str, v: &Value, secret: bool) -> Value {
    if secret {
        return Value::String(CHANGED.into());
    }
    if free_text(key) {
        return described(v);
    }
    let mut v = v.clone();
    redact(&mut v);
    strip_urls(&mut v);
    v
}

fn dotted(path: &str, k: &str) -> String {
    if path.is_empty() {
        k.to_string()
    } else {
        format!("{path}.{k}")
    }
}

/// The settings fields that differ between `before` and `after`, by dotted
/// path (`accounts.audit_days`), with their new values as [`field_value`]
/// has them. An array is one field. For a write whose settings change is a
/// side effect (`hold_set`, the switch): whatever else moved the settings
/// between the two snapshots is in it too (D85).
pub fn settings_fields(
    before: &crate::config::Settings,
    after: &crate::config::Settings,
) -> Map<String, Value> {
    let (Ok(b), Ok(a)) = (serde_json::to_value(before), serde_json::to_value(after)) else {
        return Map::new();
    };
    let mut out = Map::new();
    diff("", "", &b, &a, false, &mut out);
    out
}

fn diff(path: &str, key: &str, b: &Value, a: &Value, secret: bool, out: &mut Map<String, Value>) {
    if b == a {
        return;
    }
    match (b, a) {
        (Value::Object(bm), Value::Object(am)) if !free_text(key) => {
            let keys: BTreeSet<&String> = bm.keys().chain(am.keys()).collect();
            for k in keys {
                let null = Value::Null;
                diff(
                    &dotted(path, k),
                    k,
                    bm.get(k).unwrap_or(&null),
                    am.get(k).unwrap_or(&null),
                    secret || is_secret(k),
                    out,
                );
            }
        }
        _ => {
            out.insert(path.to_string(), field_value(key, a, secret));
        }
    }
}

/// The fields a settings patch asks for, by dotted path, as
/// [`settings_fields`] names them: a secret's empty value (which keeps the
/// stored one) is left out, a `clear_*` flag reads as its field
/// `"removed"`.
pub fn patch_fields(args: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    walk_patch("", args, &mut out);
    out
}

fn walk_patch(path: &str, m: &Map<String, Value>, out: &mut Map<String, Value>) {
    for (k, v) in m {
        if let Some(field) = k.strip_prefix("clear_") {
            match v {
                Value::Bool(true) => {
                    out.insert(dotted(path, field), "removed".into());
                }
                Value::Array(names) => {
                    for n in names.iter().filter_map(Value::as_str) {
                        out.insert(dotted(&dotted(path, field), n), "removed".into());
                    }
                }
                _ => {}
            }
            continue;
        }
        let p = dotted(path, k);
        match v {
            Value::Object(inner) if !free_text(k) => {
                if is_secret(k) {
                    // A map of secrets (`forge_tokens` by host): each named,
                    // none quoted.
                    for (name, secret) in inner {
                        if secret != &Value::String(String::new()) {
                            out.insert(dotted(&p, name), CHANGED.into());
                        }
                    }
                } else {
                    walk_patch(&p, inner, out);
                }
            }
            Value::String(s) if s.is_empty() && is_secret(k) => {}
            _ => {
                out.insert(p, field_value(k, v, is_secret(k)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_keeps_names_and_drops_content_and_secrets() {
        let args = json!({
            "id": 7, "name": "spike", "api_key": "sk-test-0000", "prompt": "hello",
            "enabled": false, "extra": { "id": 1 },
        });
        let got = identity(args.as_object().unwrap());
        assert_eq!(
            Value::Object(got),
            json!({ "id": 7, "name": "spike", "enabled": false })
        );
    }

    #[test]
    fn a_secret_reads_changed_at_any_depth() {
        let mut v = json!({ "a": { "api_key": "sk-x", "b": [ { "password": "p" } ] }, "c": 1 });
        redact(&mut v);
        assert_eq!(
            v,
            json!({ "a": { "api_key": CHANGED, "b": [ { "password": CHANGED } ] }, "c": 1 })
        );
    }

    #[test]
    fn a_leaf_that_becomes_an_object_is_redacted_inside() {
        let b = json!({ "mcp": null, "list": [] });
        let a = json!({ "mcp": { "url": "https://u:p@mcp.example/", "headers": { "x": "s" } },
                        "list": [ { "api_key": "sk-x" } ] });
        let mut out = Map::new();
        diff("", "", &b, &a, false, &mut out);
        assert_eq!(
            Value::Object(out),
            json!({ "mcp": { "url": "https://mcp.example/", "headers": CHANGED },
                    "list": [ { "api_key": CHANGED } ] })
        );
    }

    #[test]
    fn free_text_reads_as_its_length_and_a_url_loses_its_sign_in() {
        let args = json!({
            "chat_system_prompt": "You are a pirate.\nArr.",
            "realtime": { "speech_instructions": "slowly" },
            "router": { "extra_run_args": ["--api-key", "sk-x"] },
            "public_url": "https://admin:hunter2@lmgw.example:8443",
            "hf_token": "", "clear_update_token": true,
            "forge_tokens": { "git.example": "glpat-x", "other.example": "" },
            "clear_forge_tokens": ["old.example"],
        });
        let got = patch_fields(args.as_object().unwrap());
        assert_eq!(
            Value::Object(got),
            json!({
                "chat_system_prompt": "changed (22 characters)",
                "realtime.speech_instructions": "changed (6 characters)",
                "router.extra_run_args": "changed (2 arguments)",
                "public_url": "https://lmgw.example:8443",
                "update_token": "removed",
                "forge_tokens.git.example": CHANGED,
                "forge_tokens.old.example": "removed",
            })
        );
        assert_eq!(strip_userinfo("not a url @ all"), "not a url @ all");
        assert_eq!(strip_userinfo("http://h/p@q"), "http://h/p@q");
    }

    #[test]
    fn a_settings_save_names_its_fields() {
        let before = crate::config::Settings::default();
        let mut after = before.clone();
        after.accounts.audit_days = 30;
        after.hf_token = "hf_example_0000".into();
        let got = settings_fields(&before, &after);
        assert_eq!(got.get("accounts.audit_days"), Some(&json!(30)));
        assert_eq!(got.get("hf_token"), Some(&json!(CHANGED)));
        assert_eq!(got.len(), 2, "{got:?}");
    }
}
