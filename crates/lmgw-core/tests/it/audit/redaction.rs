//! The redaction list (§5.8): every name the API document flags as a
//! secret, so a field flagged there is redacted in every entry; and a
//! request-body or settings field whose name reads as a credential fails
//! here until it is flagged or named below as no secret.

use std::collections::BTreeSet;

use super::*;
use lmgw_core::audit::redact::{secret_names, CHANGED};
use lmgw_core::state::AppState;

/// Names that read as a credential and are none, each with why. A switch or
/// a fraction is never one (a field typed so is not asked about); an
/// integer can be (a PIN), so integers are asked about too.
const NOT_SECRET: &[(&str, &str)] = &[
    (
        "public_key",
        "a device's enrolment public key: public by definition (device link design §3.4)",
    ),
    (
        "scope_key",
        "a price row's scope: the upstream or model it applies to",
    ),
    (
        "clear_forge_tokens",
        "the forge hosts whose token a settings save removes: host names, not the tokens",
    ),
    (
        "max_tokens",
        "a count of model tokens (a schema without a single type, so not read as a count)",
    ),
    (
        "token_minutes",
        "how long a device's pairing token lives, in minutes (devices settings)",
    ),
    (
        "header_timeout_s",
        "the device listener's wait for a request's headers, in seconds",
    ),
    (
        "unauthenticated_max",
        "the device listener's bound on connections that have not shown a certificate yet",
    ),
    (
        "unauthenticated_per_peer",
        "the same bound, per client address",
    ),
    (
        "unauthenticated_timeout_s",
        "how long such a connection may stay, in seconds",
    ),
    (
        "pin",
        "an engine build's pinned commit (a git hash), not a PIN",
    ),
    (
        "self_admin_token",
        "a stored setting no request writes: every start moves it into the owner:self-admin \
         row and clears it (principals §3.7)",
    ),
];

/// Whether a schema is typed as a switch or a fraction only (with `null`):
/// no credential is one. An integer is asked about: a PIN is one.
fn never_secret(schema: &Value) -> bool {
    let plain = |t: &str| matches!(t, "number" | "boolean" | "null");
    match &schema["type"] {
        Value::String(t) => plain(t),
        Value::Array(ts) => ts.iter().all(|t| t.as_str().is_some_and(plain)),
        _ => false,
    }
}

/// A count of model tokens (`max_tokens`, `chat_kb_budget_tokens`): an
/// integer named for the tokens it counts, not for a token it holds.
fn token_count(name: &str, schema: &Value) -> bool {
    let int = |t: &str| t == "integer";
    let typed = match &schema["type"] {
        Value::String(t) => int(t),
        Value::Array(ts) => ts.iter().any(|t| t.as_str().is_some_and(int)),
        _ => false,
    };
    typed && name.ends_with("tokens")
}

/// Does `name` read as a credential? Long words anywhere in it, short ones
/// (`pin`, `otp`, ...) only as a whole word of a snake- or kebab-case name,
/// `key` as the name or its last word (`key_id` names a key).
fn reads_as_secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let anywhere = [
        "password",
        "passwd",
        "passcode",
        "passphrase",
        "token",
        "secret",
        "api_key",
        "apikey",
        "credential",
        "private",
        "cookie",
        "auth",
        "bearer",
        "header",
        "env",
        "nonce",
        "signature",
        "hmac",
    ];
    let words = ["pin", "otp", "pwd", "pass", "dsn", "salt"];
    anywhere.iter().any(|w| n.contains(w))
        || n.split(['_', '-', '.']).any(|w| words.contains(&w))
        || n == "key"
        || n.ends_with("_key")
}

/// Every property of `schema` a value can carry, at any depth, `$ref`s
/// followed in `defs`, as `(name, its schema)`.
fn properties(
    defs: &Value,
    v: &Value,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<(String, Value)>,
) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("$ref") {
                let name = r.rsplit('/').next().unwrap().to_string();
                if seen.insert(name.clone()) {
                    properties(defs, &defs[&name], seen, out);
                }
            }
            if let Some(Value::Object(props)) = map.get("properties") {
                out.extend(props.iter().map(|(k, p)| (k.clone(), p.clone())));
            }
            for v in map.values() {
                properties(defs, v, seen, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| properties(defs, v, seen, out)),
        _ => {}
    }
}

/// The names worth asking about: not a switch or a fraction, not a count
/// of tokens.
fn asked(props: Vec<(String, Value)>) -> impl Iterator<Item = String> {
    props
        .into_iter()
        .filter(|(k, p)| !never_secret(p) && !token_count(k, p))
        .map(|(k, _)| k)
}

/// Every property name a request body can carry, at any depth, `$ref`s
/// followed, and every parameter's name.
fn request_names(doc: &Value) -> BTreeSet<String> {
    let defs = &doc["components"]["schemas"];
    let mut out = BTreeSet::new();
    for methods in doc["paths"].as_object().unwrap().values() {
        for op in methods.as_object().unwrap().values() {
            let mut props = Vec::new();
            properties(defs, &op["requestBody"], &mut BTreeSet::new(), &mut props);
            out.extend(asked(props));
            for p in op["parameters"].as_array().into_iter().flatten() {
                if let (Some(n), false) = (p["name"].as_str(), never_secret(&p["schema"])) {
                    out.insert(n.to_string());
                }
            }
        }
    }
    out
}

/// Every field of the settings as stored, at any depth, from their JSON
/// schema: an optional struct, a list of structs or a map's values are
/// walked whether or not the defaults hold one.
fn settings_names() -> BTreeSet<String> {
    let schema = serde_json::to_value(schemars::schema_for!(lmgw_core::config::Settings)).unwrap();
    let mut props = Vec::new();
    properties(&schema["$defs"], &schema, &mut BTreeSet::new(), &mut props);
    assert!(
        props.iter().any(|(k, _)| k == "audit_days"),
        "the settings schema is walked into its structs"
    );
    asked(props).collect()
}

#[test]
fn the_guard_reads_a_pin_and_a_nested_secret() {
    assert!(reads_as_secret("unlock_pin") && reads_as_secret("smtp_pass"));
    assert!(reads_as_secret("webhook_signature") && reads_as_secret("sentry_dsn"));
    assert!(!reads_as_secret("pinned") && !reads_as_secret("bypass") && !reads_as_secret("keep"));
    let schema = serde_json::json!({ "type": "integer" });
    assert!(!never_secret(&schema), "an integer is asked about");
    assert!(token_count("max_tokens", &schema));
}

#[test]
fn the_list_is_the_documents_secret_flags() {
    let names = secret_names();
    for want in [
        "password",
        "current",
        "token",
        "api_key",
        "extra_headers",
        "hf_token",
        "update_token",
        "forge_tokens",
        "env",
        "headers",
        "values",
    ] {
        assert!(
            names.contains(want),
            "{want} is flagged in the document: {names:?}"
        );
    }
}

#[test]
fn a_field_that_reads_as_a_credential_is_flagged_or_named_as_none() {
    let doc = lmgw_core::openapi::admin_doc();
    let names = secret_names();
    let fine: BTreeSet<&str> = NOT_SECRET.iter().map(|(n, _)| *n).collect();
    let mut unflagged: Vec<String> = request_names(doc)
        .into_iter()
        .chain(settings_names())
        .filter(|n| reads_as_secret(n) && !names.contains(n) && !fine.contains(n.as_str()))
        .collect();
    unflagged.sort();
    unflagged.dedup();
    assert!(
        unflagged.is_empty(),
        "these request or settings fields read as a credential but the API document does not \
         flag them x-lmgw-secret, so the audit log would store their values: flag them where \
         they are defined (openapi_ext::secret), or name them in NOT_SECRET with why: \
         {unflagged:?}"
    );
}

#[tokio::test]
async fn no_secret_reaches_an_entry() {
    let state = AppState::init_for_tests().await.unwrap();
    let gw = serve(state.clone()).await;
    let session = turn_on(&gw).await;
    let sentinels = [
        "sk-spike-wp7-upstream",
        "hf_spike_wp7_token",
        "spike-wp7-update-token",
        "spike-wp7-forge-token",
        "spike wp7 password 0000",
        "spike-wp7-mcp-env",
        "spike-wp7-mcp-header",
    ];
    let calls = [
        (
            "upstream_set",
            json!({ "action": "create", "name": "spike-wp7", "kind": "openai_compatible",
                    "base_url": "https://api.example/v1", "api_key": sentinels[0] }),
        ),
        (
            "settings_set_full",
            json!({ "hf_token": sentinels[1], "update_token": sentinels[2],
                    "forge_tokens": { "git.example": sentinels[3] } }),
        ),
        ("account_password_set", json!({ "password": sentinels[4] })),
        (
            "mcp_server_set",
            json!({ "action": "create", "name": "spike-wp7", "command": "true",
                    "env": { "SPIKE": sentinels[5] }, "headers": { "x-spike": sentinels[6] } }),
        ),
    ];
    let before = count(&state).await;
    for (name, args) in calls {
        let (status, body) = op_as(&gw, &session, name, args).await;
        assert!(status < 500, "{name}: {status} {body}");
    }
    assert_eq!(count(&state).await, before + 4);
    let log = super::whole_log(&state).await;
    for s in sentinels {
        assert!(!log.contains(s), "{s} reached the audit log: {log}");
    }
    let settings = with_action(&gw, "settings_set_full").await;
    assert_eq!(
        settings[0]["target"]["fields"]["hf_token"], CHANGED,
        "{settings:?}"
    );
    assert_eq!(
        settings[0]["target"]["fields"]["forge_tokens.git.example"],
        CHANGED
    );
}
