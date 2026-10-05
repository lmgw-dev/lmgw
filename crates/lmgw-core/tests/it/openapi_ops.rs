//! The op plane's structure (api-docs design §4.7, §7.1).
//!
//! WP1's share: the vocabulary itself (`op_names_match_the_dispatcher_arms`)
//! and the gate that refuses anything outside it
//! (`an_unlisted_op_is_refused_before_dispatch`). `every_listed_op_is_
//! documented_and_vice_versa`, `tool_and_op_arguments_agree_except_listed_
//! divergences` and `writes_agree_with_the_tool` are WP5's, now that
//! `ops/table.rs`'s `table()` has its 83 entries.

use crate::common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

use lmgw_api_types::openapi_ext as ext;
use lmgw_core::mcp::selfadmin;
use lmgw_core::openapi::{admin_doc, DIVERGENCES};
use lmgw_core::web::op_names;

use common::Gw;

// ---------------------------------------------------------------------------
// A small, dependency-free Rust-source scanner — `route_walk.rs`'s own style
// (brace/paren matching, no `regex`), scoped to one function's body at a time
// so a match arm's *own* string arguments (`arg_str(&args, "repo")`) are
// never mistaken for a dispatch pattern.
// ---------------------------------------------------------------------------

/// The body of the function whose signature starts with `anchor` (e.g.
/// `"fn op("`), braces included — from the parameter list's closing `)` to
/// the function body's matching `}`.
fn function_body<'a>(text: &'a str, anchor: &str) -> &'a str {
    let start = text
        .find(anchor)
        .unwrap_or_else(|| panic!("no function matching {anchor:?} found"));
    let params_open = start + anchor.len() - 1; // the '(' `anchor` ends with
    let params_close = matching(text, params_open, b'(', b')');
    let body_open = text[params_close..]
        .find('{')
        .map(|i| params_close + i)
        .expect("a function signature is followed by a `{`");
    let body_close = matching(text, body_open, b'{', b'}');
    &text[body_open..=body_close]
}

/// The index of the `close` byte matching the `open` byte at `open_idx`,
/// skipping string/char literals and line comments so a `)` or `}` inside
/// either is not counted.
fn matching(text: &str, open_idx: usize, open: u8, close: u8) -> usize {
    let bytes = text.as_bytes();
    assert_eq!(bytes[open_idx], open);
    let mut depth = 1usize;
    let mut i = open_idx + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'\'' if i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() => {
                // A char literal (`'a'`) vs a lifetime (`'a` with no closing
                // quote) — only consume it if a closing `'` follows shortly.
                if let Some(rel) = text[i + 1..(i + 4).min(bytes.len())].find('\'') {
                    i += 1 + rel;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b if b == open => depth += 1,
            b if b == close => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unbalanced {}/{}", open as char, close as char);
}

/// Every string literal in `body` that is a match arm's own pattern: one
/// followed (after whitespace) by `=>`, by `|` (an or-pattern:
/// `"aux_model_set" | "embed_model_set" =>`), or by an `if` guard
/// (`"x" if cond =>`, review R2 #6).
fn arm_names(body: &str) -> BTreeSet<&str> {
    let mut out = BTreeSet::new();
    let mut i = 0;
    while let Some(rel) = body[i..].find('"') {
        let open = i + rel;
        let Some(close_rel) = body[open + 1..].find('"') else {
            break;
        };
        let close = open + 1 + close_rel;
        let name = &body[open + 1..close];
        let after = body[close + 1..].trim_start();
        let guard = after.starts_with("if")
            && after[2..].starts_with(|c: char| c.is_whitespace() || c == '(');
        if after.starts_with("=>") || after.starts_with('|') || guard {
            out.insert(name);
        }
        i = close + 1;
    }
    out
}

/// Every literal inside a `matches!(name, "a" | "b")` — the last one is
/// followed by `)`, which [`arm_names`] alone would not count (review R2 #6).
fn matches_names(body: &str) -> BTreeSet<&str> {
    let mut out = BTreeSet::new();
    let needle = "matches!(name,";
    let mut i = 0;
    while let Some(rel) = body[i..].find(needle) {
        let open = i + rel + needle.len() - 1 - "name,".len(); // the '('
        let close = matching(body, open, b'(', b')');
        let inner = &body[open + 1 + "name,".len()..close];
        let mut j = 0;
        while let Some(q) = inner[j..].find('"') {
            let a = j + q + 1;
            let Some(b_rel) = inner[a..].find('"') else {
                break;
            };
            out.insert(&inner[a..a + b_rel]);
            j = a + b_rel + 1;
        }
        i = close;
    }
    out
}

/// Every `name == "x"` and `"x" == name` in `body` — the special-cased
/// single-name checks (`settings_set_full`), in any of the three dispatchers.
fn name_eq_names(body: &str) -> BTreeSet<&str> {
    let mut out = BTreeSet::new();
    let needle = "name == \"";
    let mut i = 0;
    while let Some(rel) = body[i..].find(needle) {
        let open = i + rel + needle.len();
        let Some(close_rel) = body[open..].find('"') else {
            break;
        };
        out.insert(&body[open..open + close_rel]);
        i = open + close_rel;
    }
    let needle = "\" == name";
    let mut i = 0;
    while let Some(rel) = body[i..].find(needle) {
        let close = i + rel;
        if let Some(open) = body[..close].rfind('"') {
            out.insert(&body[open + 1..close]);
        }
        i = close + needle.len();
    }
    out
}

#[test]
fn the_scanner_sees_guards_matches_and_equality_checks() {
    let body = r#"{
        if name == "eq_left" { return x; }
        if "eq_right" == name { return y; }
        if matches!(name, "m_first" | "m_last") { return z; }
        match name {
            "plain" => a(),
            "or_a" | "or_b" => b(),
            "guarded" if cond => c(),
            "guarded_paren" if(cond) => c(),
            "not_an_arm_arg" => d(arg("inner")),
            _ => e(),
        }
    }"#;
    assert_eq!(
        arm_names(body),
        BTreeSet::from([
            "plain",
            "or_a",
            "or_b",
            "guarded",
            "guarded_paren",
            "not_an_arm_arg",
            "m_first",
        ])
    );
    assert_eq!(matches_names(body), BTreeSet::from(["m_first", "m_last"]));
    assert_eq!(name_eq_names(body), BTreeSet::from(["eq_left", "eq_right"]));
}

fn set(names: &'static [&'static str]) -> BTreeSet<&'static str> {
    names.iter().copied().collect()
}

/// Every op name `body` answers: its match arms, its `matches!(name, ..)`
/// lists and its `name == "…"` checks.
fn dispatched(body: &str) -> BTreeSet<&str> {
    let mut out = arm_names(body);
    out.extend(matches_names(body));
    out.extend(name_eq_names(body));
    out
}

#[test]
fn op_names_match_the_dispatcher_arms() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");

    let api_rs = std::fs::read_to_string(src.join("web/api.rs")).unwrap();
    let op_body = function_body(&api_rs, "fn op(");
    let mut main_found = arm_names(op_body);
    main_found.extend(matches_names(op_body));
    let settings_found = name_eq_names(op_body);
    assert_eq!(
        main_found,
        set(op_names::MAIN_OPS),
        "web/api.rs's op() match arms and web::op_names::MAIN_OPS have drifted"
    );
    assert_eq!(
        settings_found,
        set(op_names::SETTINGS_OPS),
        "web/api.rs's `name == \"...\"` checks and web::op_names::SETTINGS_OPS have drifted"
    );

    let agents_rs = std::fs::read_to_string(src.join("web/api_agents.rs")).unwrap();
    let agent_found = dispatched(function_body(&agents_rs, "fn op("));
    assert_eq!(
        agent_found,
        set(op_names::AGENT_OPS),
        "web/api_agents.rs's op() and web::op_names::AGENT_OPS have drifted"
    );

    let settings_rs = std::fs::read_to_string(src.join("web/api_settings.rs")).unwrap();
    let key_found = dispatched(function_body(&settings_rs, "fn key_op("));
    assert_eq!(
        key_found,
        set(op_names::KEY_OPS),
        "web/api_settings.rs's key_op() and web::op_names::KEY_OPS have drifted"
    );
}

/// The gate answers before any dispatcher does (§4.7). Every name a
/// dispatcher arm answers is listed (`op_names_match_the_dispatcher_arms`),
/// so there is no "answered but unlisted" name to send; what tells the gate
/// apart from the main `match`'s own fall-through is its wording — without
/// the gate this probe would still get a 400 `unknown op`, but not this one
/// (review R2 #6).
#[tokio::test]
async fn an_unlisted_op_is_refused_before_dispatch() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let gw: Gw = common::serve(state).await;

    for probe in ["route-walk-probe", "agent_nonsense", "key_nonsense"] {
        let resp = gw
            .client()
            .post(format!("{gw}/api/op/{probe}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{probe}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["code"], "op_failed", "{probe}");
        assert_eq!(
            body["message"].as_str(),
            op_names::refuse_unlisted(probe).as_deref(),
            "{probe}: {body:#?}"
        );
    }
}

/// Review R2 #2: an op documented with no `requestBody` (`OpArgs::NoArgs`,
/// `AliasOf`) is called with no body — that is what the tester's Send and its
/// copied curl do — and used to get `Json<_>`'s `415` for the missing
/// `Content-Type`. No body is `{}` now; a non-empty body still goes through
/// the JSON extractor's own checks.
#[tokio::test]
async fn a_no_argument_op_takes_no_body() {
    let state = lmgw_core::state::AppState::init_for_tests().await.unwrap();
    let gw: Gw = common::serve(state).await;
    let url = format!("{gw}/api/op/builds");
    let client = gw.client();

    // No body, no Content-Type.
    let bare = client.post(&url).send().await.unwrap();
    assert_eq!(bare.status(), 200, "{:?}", bare.text().await);

    // An empty body that does say JSON.
    let empty_json = client
        .post(&url)
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(empty_json.status(), 200);
    let body: Value = empty_json.json().await.unwrap();
    assert!(body["builds"].is_array(), "{body:#?}");

    // A malformed non-empty body is still refused as before.
    let malformed = client
        .post(&url)
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), 400);

    // A non-empty body with no Content-Type is still `Json<_>`'s 415.
    let untyped = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(untyped.status(), 415);

    // And a JSON value that is not an object is still refused.
    let not_object = client
        .post(&url)
        .header("content-type", "application/json")
        .body("[]")
        .send()
        .await
        .unwrap();
    assert_eq!(not_object.status(), 422);
}

// ---------------------------------------------------------------------------
// WP5: the op table itself.
// ---------------------------------------------------------------------------

/// Every `POST /api/op/{name}` operation in the admin document, keyed by its
/// `x-lmgw-op` name.
fn op_operations(doc: &Value) -> Vec<(String, Value)> {
    doc["paths"]
        .as_object()
        .unwrap()
        .values()
        .filter_map(|methods| methods.get("post"))
        .filter_map(|op| {
            op.get(ext::OP)
                .and_then(Value::as_str)
                .map(|name| (name.to_string(), op.clone()))
        })
        .collect()
}

#[test]
fn every_listed_op_is_documented_and_vice_versa() {
    let doc = admin_doc();
    let documented: BTreeSet<String> = op_operations(doc).into_iter().map(|(n, _)| n).collect();
    let listed: BTreeSet<String> = op_names::all().map(String::from).collect();
    assert_eq!(
        documented, listed,
        "the doc's x-lmgw-op set and web::op_names::all() have drifted"
    );
    assert_eq!(listed.len(), 83, "web::op_names::all() should name 83 ops");
}

/// The property *names* a JSON Schema object declares (`properties`'s keys) —
/// deliberately not the full property *definitions*: a struct-derived
/// property differs from the tool's flat one in shape alone (`Option<T>`
/// unions in an optional field, say), which is noise this comparison must
/// not trip over — only whether the argument exists at all is what "tool
/// props ⊆ op props" (§4.7) asks.
fn prop_names(schema: &Value) -> BTreeSet<String> {
    schema["properties"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// The property names a schema's `required` array lists.
fn required_names(schema: &Value) -> BTreeSet<String> {
    schema["required"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// A property schema's JSON types with `null` left out (an `Option<T>`
/// field's union is shape noise), or `None` when it declares no `type` of its
/// own (a `$ref`, an `anyOf`).
fn non_null_types(prop: &Value) -> Option<BTreeSet<String>> {
    let types: BTreeSet<String> = match &prop["type"] {
        Value::String(t) => BTreeSet::from([t.clone()]),
        Value::Array(ts) => ts
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
        _ => return None,
    };
    Some(types.into_iter().filter(|t| t != "null").collect())
}

/// Whether `name` still differs between the tool's and the op's schema: it
/// is on one side only; both declare a `type` and the types differ; the
/// `enum`s differ (one side having none counts); or its required-ness
/// differs — that last only where the op's schema has a `required` of its
/// own, which a struct-derived one never does (`#[serde(default)]`).
fn prop_differs(name: &str, tool_schema: &Value, op_schema: &Value) -> bool {
    let tool = tool_schema["properties"].get(name);
    let op = op_schema["properties"].get(name);
    let (Some(tool), Some(op)) = (tool, op) else {
        return tool.is_some() != op.is_some();
    };
    let types_differ = matches!(
        (non_null_types(tool), non_null_types(op)),
        (Some(a), Some(b)) if a != b
    );
    let enums_differ = tool.get("enum") != op.get("enum");
    let op_required = required_names(op_schema);
    let required_differs = !op_required.is_empty()
        && required_names(tool_schema).contains(name) != op_required.contains(name);
    types_differ || enums_differ || required_differs
}

fn op_request_schema(operation: &Value) -> Option<&Value> {
    operation.pointer("/requestBody/content/application~1json/schema")
}

#[test]
fn tool_and_op_arguments_agree_except_listed_divergences() {
    let doc = admin_doc();
    let catalog = selfadmin::full_catalog();

    let mut checked = 0;
    for (op_name, operation) in op_operations(doc) {
        let Some(tool_name) = operation.get(ext::TOOL).and_then(Value::as_str) else {
            continue;
        };
        checked += 1;
        let (tool_entry, _) = catalog
            .iter()
            .find(|(e, _)| e["name"] == tool_name)
            .unwrap_or_else(|| {
                panic!("op '{op_name}' names tool '{tool_name}', not in the self-admin catalog")
            });
        let tool_schema = &tool_entry["inputSchema"];
        let tool_props = prop_names(tool_schema);

        let op_schema = op_request_schema(&operation);
        let op_props = op_schema.map(prop_names).unwrap_or_default();

        if let Some(d) = DIVERGENCES.iter().find(|d| d.op == op_name) {
            // Still genuinely different — not a stale claim (§7.1 "every
            // divergence is still true") — on each argument the entry names
            // (review R2 #7: "the two schemas differ somewhere" held for
            // every struct-derived op, divergent or not).
            match op_schema {
                None => assert!(
                    d.props.is_empty(),
                    "'{op_name}' takes no body; its DIVERGENCES entry should name no argument"
                ),
                Some(op_schema) => {
                    assert!(
                        !d.props.is_empty(),
                        "'{op_name}': a DIVERGENCES entry names the arguments it is about"
                    );
                    for prop in d.props {
                        assert!(
                            prop_differs(prop, tool_schema, op_schema),
                            "'{op_name}' is listed in DIVERGENCES for '{prop}', but its tool \
                             '{tool_name}' and its op now agree on it — remove the stale claim"
                        );
                    }
                }
            }
        } else {
            assert!(
                tool_props.is_subset(&op_props),
                "'{op_name}': tool '{tool_name}' names {tool_props:?}, the op only takes \
                 {op_props:?} — either the op struct lost a field, or this is a real \
                 divergence that belongs in DIVERGENCES"
            );
        }
    }
    assert!(checked >= 20, "only checked {checked} tool-sharing ops");

    // Every divergence names a real op — a typo here would otherwise
    // silently exempt nothing from the strict check above.
    for d in DIVERGENCES {
        assert!(
            op_names::is_op(d.op),
            "DIVERGENCES names '{}', which is not a real op",
            d.op
        );
    }
}

#[test]
fn writes_agree_with_the_tool() {
    let doc = admin_doc();
    let catalog = selfadmin::full_catalog();

    // No op's `writes` legitimately differs from its tool's today — every
    // Table A/B row was checked by hand against its tool's `writes` flag.
    // Kept as a real (currently empty) list, per §4.7 "the test allows an
    // explicit exception list", so a genuine future exception has somewhere
    // to go without weakening the assertion below for everything else.
    const EXCEPTIONS: &[&str] = &[];

    let mut checked = 0;
    for (op_name, operation) in op_operations(doc) {
        let Some(tool_name) = operation.get(ext::TOOL).and_then(Value::as_str) else {
            continue;
        };
        if EXCEPTIONS.contains(&op_name.as_str()) {
            continue;
        }
        let (_, tool_writes) = catalog
            .iter()
            .find(|(e, _)| e["name"] == tool_name)
            .unwrap_or_else(|| {
                panic!("op '{op_name}' names tool '{tool_name}', not in the self-admin catalog")
            });
        let op_writes = operation[ext::WRITES]
            .as_bool()
            .unwrap_or_else(|| panic!("'{op_name}' has no {}", ext::WRITES));
        assert_eq!(
            op_writes, *tool_writes,
            "'{op_name}' writes={op_writes} but its tool '{tool_name}' writes={tool_writes}"
        );
        checked += 1;
    }
    assert!(checked >= 20, "only checked {checked} tool-sharing ops");
}

/// Review R2 #8: every op whose answer carries a plaintext credential says
/// so — the whole set, pinned — and the flags a reader acts on are right
/// where the reviewer found them wrong.
#[test]
fn reveals_secret_and_writes_flags_are_pinned() {
    let doc = admin_doc();
    let revealing: BTreeSet<String> = op_operations(doc)
        .into_iter()
        .filter(|(_, op)| op[ext::REVEALS_SECRET] == true)
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        revealing,
        BTreeSet::from(
            [
                "agent_token_get",
                "agent_token_rotate",
                "key_create",
                "key_reveal",
                "key_rotate",
            ]
            .map(String::from)
        )
    );

    // `token::ensure` mints a missing token and rewrites its scope.
    assert_eq!(
        doc["paths"]["/api/op/agent_token_get"]["post"][ext::WRITES],
        true
    );
    // A POST only for its body.
    assert_eq!(doc["paths"]["/api/docs/search"]["post"][ext::WRITES], false);

    // The ledger body is JSON or NDJSON text, never a JSON string.
    let events =
        &doc["paths"]["/api/agents/runs/{job_id}/events"]["post"]["requestBody"]["content"];
    assert!(events["application/x-ndjson"].is_object(), "{events:#?}");
    let json_schema = &doc["components"]["schemas"]["AgentRunEventsBody"];
    assert_eq!(json_schema["type"], serde_json::json!(["object", "array"]));
}
