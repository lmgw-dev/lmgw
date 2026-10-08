//! Drift guards over the *whole* document (api-docs design §7.1, §7.2): every
//! `CAPABILITY_TABLE` row is documented or excluded, both ways; the served
//! documents follow their capabilities; every typed extractor is referenced;
//! every example validates. Also `tests/it/openapi_live.rs`'s live GET
//! validation. WP8.
//!
//! Every other openapi suite (`openapi_{ops,headers,v1,dashboard}.rs`) checks
//! one plane or one rule; this file is the one that walks the *entire*
//! `CAPABILITY_TABLE` and the *entire* built document against each other, so
//! a route that no plane owns (or a plane that documents a route the table
//! does not have) cannot go unnoticed just because no single-plane test
//! happened to cover it.

use crate::common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use lmgw_api_types::openapi_ext as ext;
use lmgw_core::config::hash_api_key;
use lmgw_core::openapi::{admin_doc, lmgw_endpoints, v1_doc};
use lmgw_core::server::CAPABILITY_TABLE;
use lmgw_core::state::{AppState, SharedState};

/// `{*id}` → `{id}`, the same translation `openapi::params::openapi_path`
/// applies — duplicated here (that function is `pub(crate)` to the `openapi`
/// module) rather than exposed just for this test.
fn openapi_path(path: &str) -> String {
    path.replace("{*", "{")
}

/// The one `CAPABILITY_TABLE` row every op is expanded from — never itself a
/// documented path (the document has one concrete `/api/op/<name>` path per
/// op instead), so it is its own bucket rather than falling out of "documented
/// or excluded".
const OP_ROW: (&str, &str) = ("POST", "/api/op/{name}");

// ---------------------------------------------------------------------------
// every_capability_row_is_documented_or_excluded / its reverse
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Undoc {
    method: String,
    path: String,
    reason: String,
}

fn undocumented(doc: &Value) -> Vec<Undoc> {
    doc[ext::UNDOCUMENTED]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|u| Undoc {
            method: u["method"].as_str().unwrap_or_default().to_string(),
            path: u["path"].as_str().unwrap_or_default().to_string(),
            reason: u["reason"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

fn is_excluded(rows: &[Undoc], method: &str, path: &str) -> bool {
    rows.iter()
        .any(|u| u.path == path && (u.method == method || u.method == "*"))
}

/// Every `(method, openapi_path)` the admin document actually serves, with
/// its `x-lmgw-capability` — op-expanded `/api/op/<name>` paths excluded
/// (§4.2's second bucket, checked on its own below).
fn documented_non_op_rows(doc: &Value) -> BTreeMap<(String, String), String> {
    let mut out = BTreeMap::new();
    for (path, methods) in doc["paths"].as_object().unwrap() {
        if path.starts_with("/api/op/") {
            continue;
        }
        for (method, op) in methods.as_object().unwrap() {
            if !matches!(
                method.as_str(),
                "get" | "put" | "post" | "delete" | "options" | "head" | "patch" | "trace"
            ) {
                continue;
            }
            let cap = op[ext::CAPABILITY]
                .as_str()
                .unwrap_or_else(|| panic!("{method} {path} has no {}", ext::CAPABILITY))
                .to_string();
            out.insert((method.to_ascii_uppercase(), path.clone()), cap);
        }
    }
    out
}

#[test]
fn every_capability_row_is_documented_or_excluded() {
    let doc = admin_doc();
    let undoc = undocumented(doc);
    let documented = documented_non_op_rows(doc);

    let mut checked = 0;
    for (method, path, _cap) in CAPABILITY_TABLE {
        if (*method, *path) == OP_ROW {
            continue; // the op-expansion row, its own bucket.
        }
        let norm = openapi_path(path);
        let is_doc = documented.contains_key(&(method.to_string(), norm.clone()));
        let is_exc = is_excluded(&undoc, method, path);
        assert!(
            is_doc || is_exc,
            "{method} {path}: neither documented nor excluded"
        );
        assert!(
            !(is_doc && is_exc),
            "{method} {path}: both documented and excluded"
        );
        checked += 1;
    }
    assert!(checked > 100, "only checked {checked} rows");
}

#[test]
fn every_documented_operation_is_a_capability_row() {
    let doc = admin_doc();

    // Non-op rows: normalize CAPABILITY_TABLE once, then every documented
    // (method, path) must resolve back to exactly one row, with the same
    // capability the table names.
    let mut table: BTreeMap<(String, String), &str> = BTreeMap::new();
    for (method, path, cap) in CAPABILITY_TABLE {
        if *method == "*" {
            continue; // never a DocRoute's own method.
        }
        table.insert((method.to_string(), openapi_path(path)), cap.as_str());
    }

    let op_row_cap = CAPABILITY_TABLE
        .iter()
        .find(|(m, p, _)| (*m, *p) == OP_ROW)
        .map(|(_, _, cap)| cap.as_str())
        .expect("CAPABILITY_TABLE carries the POST /api/op/{name} row");

    let mut checked = 0;
    for (path, methods) in doc["paths"].as_object().unwrap() {
        for (method, op) in methods.as_object().unwrap() {
            if !matches!(
                method.as_str(),
                "get" | "put" | "post" | "delete" | "options" | "head" | "patch" | "trace"
            ) {
                continue;
            }
            let cap = op[ext::CAPABILITY]
                .as_str()
                .unwrap_or_else(|| panic!("{method} {path} has no {}", ext::CAPABILITY));
            if let Some(name) = op.get(ext::OP).and_then(Value::as_str) {
                assert_eq!(
                    *path,
                    format!("/api/op/{name}"),
                    "op '{name}' is documented at a path that does not match its own name"
                );
                assert_eq!(
                    cap, op_row_cap,
                    "op '{name}' claims capability {cap}, but POST /api/op/{{name}} is {op_row_cap}"
                );
            } else {
                let key = (method.to_ascii_uppercase(), path.clone());
                let want = table.get(&key).unwrap_or_else(|| {
                    panic!("{method} {path} is documented but is not a CAPABILITY_TABLE row")
                });
                assert_eq!(
                    cap, *want,
                    "{method} {path}: documented capability {cap} disagrees with the table's {want}"
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 100, "only checked {checked} operations");
}

#[test]
fn exclusions_are_real_rows_with_reasons() {
    let doc = admin_doc();
    let table: BTreeSet<(String, String)> = CAPABILITY_TABLE
        .iter()
        .map(|(m, p, _)| (m.to_string(), p.to_string()))
        .collect();

    let undoc = undocumented(doc);
    assert!(!undoc.is_empty());
    for u in &undoc {
        assert!(
            !u.reason.trim().is_empty(),
            "{} {} has an empty reason",
            u.method,
            u.path
        );
        assert!(
            table.contains(&(u.method.clone(), u.path.clone())),
            "excluded {} {} names no real CAPABILITY_TABLE row",
            u.method,
            u.path
        );
    }
}

// ---------------------------------------------------------------------------
// served_docs_follow_their_capabilities
// ---------------------------------------------------------------------------

async fn insert_key(state: &SharedState, name: &str, plain: &str, kind: &str) {
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, key_plain, enabled, kind, agent_id)
         VALUES (?1, ?2, NULL, 1, ?3, NULL)",
    )
    .bind(name)
    .bind(hash_api_key(plain))
    .bind(kind)
    .execute(&state.db)
    .await
    .unwrap();
}

/// Every reachable `#/components/schemas/<name>` starting from `root`,
/// transitively through the components it reaches.
fn reachable_schemas(doc: &Value) -> BTreeSet<String> {
    fn collect_refs(value: &Value, out: &mut Vec<String>) {
        const PREFIX: &str = "#/components/schemas/";
        match value {
            Value::String(s) => {
                if let Some(name) = s.strip_prefix(PREFIX) {
                    out.push(name.to_string());
                }
            }
            Value::Object(map) => map.values().for_each(|v| collect_refs(v, out)),
            Value::Array(items) => items.iter().for_each(|v| collect_refs(v, out)),
            _ => {}
        }
    }

    let empty = Value::Object(Default::default());
    let schemas = doc["components"]["schemas"]
        .as_object()
        .unwrap_or_else(|| empty.as_object().unwrap());

    let mut refs = Vec::new();
    collect_refs(&doc["paths"], &mut refs);
    let mut reached: BTreeSet<String> = refs.into_iter().collect();
    loop {
        let mut grew = false;
        let names: Vec<String> = reached.iter().cloned().collect();
        for name in names {
            let Some(schema) = schemas.get(&name) else {
                continue;
            };
            let mut refs = Vec::new();
            collect_refs(schema, &mut refs);
            for r in refs {
                if reached.insert(r) {
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    reached
}

#[tokio::test]
async fn served_docs_follow_their_capabilities() {
    let state = AppState::init_for_tests().await.unwrap();
    let client_key = "lmgw-coverage-client".to_string();
    insert_key(&state, "coverage-client", &client_key, "key").await;
    state.reload_snapshot().await.unwrap();
    let gw = common::serve(state).await;
    let bare = reqwest::Client::new();

    // GET /api/openapi.json is Admin: owner 200, a client key 403, no
    // credential 401 (route_walk.rs's own `expected()` table: Admin refuses
    // Anonymous with 401 regardless of the auth-required toggle).
    let owner = gw
        .client()
        .get(format!("{gw}/api/openapi.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(owner.status(), 200);

    let client = bare
        .get(format!("{gw}/api/openapi.json"))
        .bearer_auth(&client_key)
        .send()
        .await
        .unwrap();
    assert_eq!(client.status(), 403);

    let anon = gw
        .anon()
        .get(format!("{gw}/api/openapi.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    // GET /v1/openapi.json is Inference: the same client key gets 200.
    let v1_resp = bare
        .get(format!("{gw}/v1/openapi.json"))
        .bearer_auth(&client_key)
        .send()
        .await
        .unwrap();
    assert_eq!(v1_resp.status(), 200);
    let v1: Value = v1_resp.json().await.unwrap();

    for (path, methods) in v1["paths"].as_object().unwrap() {
        assert!(!path.starts_with("/api"), "{path} leaked into the v1 doc");
        for (method, op) in methods.as_object().unwrap() {
            assert_eq!(
                op[ext::CAPABILITY],
                "inference",
                "{method} {path} is in the v1 doc with capability {}",
                op[ext::CAPABILITY]
            );
        }
    }

    let schemas: BTreeSet<String> = v1["components"]["schemas"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let reached = reachable_schemas(&v1);
    let unreachable: Vec<&String> = schemas.difference(&reached).collect();
    assert!(
        unreachable.is_empty(),
        "unreachable components: {unreachable:?}"
    );

    // The root is pruned too (review R2 #5, R3 #5): only the tags and the
    // security schemes its own operations use, and none of the admin
    // plane's root extensions.
    let mut used_tags = BTreeSet::new();
    let mut used_schemes = BTreeSet::new();
    for methods in v1["paths"].as_object().unwrap().values() {
        for op in methods.as_object().unwrap().values() {
            for t in op["tags"].as_array().unwrap() {
                used_tags.insert(t.as_str().unwrap().to_string());
            }
            for entry in op["security"].as_array().unwrap() {
                used_schemes.extend(entry.as_object().unwrap().keys().cloned());
            }
        }
    }
    let declared_tags: BTreeSet<String> = v1["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(declared_tags, used_tags, "v1 root tags");
    assert!(
        !declared_tags.iter().any(|t| t.starts_with("ops-")),
        "{declared_tags:?}"
    );
    let declared_schemes: BTreeSet<String> = v1["components"]["securitySchemes"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(declared_schemes, used_schemes, "v1 security schemes");
    assert!(!declared_schemes.contains("adminToken"));
    assert!(
        v1.get(ext::UNDOCUMENTED).is_none(),
        "undocumented list in v1"
    );
    assert!(v1.get(ext::PRINCIPALS).is_none(), "principals matrix in v1");

    // The admin document keeps both: the page reads the matrix, and the
    // undocumented list is the admin plane's honest inventory (spec §12).
    let admin: Value = owner.json().await.unwrap();
    assert!(admin[ext::UNDOCUMENTED]
        .as_array()
        .is_some_and(|a| !a.is_empty()));
    assert!(admin[ext::PRINCIPALS].is_object());
    assert!(admin["components"]["securitySchemes"]["adminToken"].is_object());
}

// ---------------------------------------------------------------------------
// lmgw_endpoints_are_documented_inference_paths
// ---------------------------------------------------------------------------

#[test]
fn lmgw_endpoints_are_documented_inference_paths() {
    let v1 = v1_doc();
    let paths = v1["paths"].as_object().unwrap();
    let endpoints = lmgw_endpoints();

    let mut checked = 0;
    for group in ["openai", "anthropic", "other"] {
        for path in endpoints[group].as_array().unwrap() {
            let path = path.as_str().unwrap();
            assert!(
                paths.contains_key(path),
                "lmgw.endpoints.{group} names {path}, which is not in the v1 doc"
            );
            checked += 1;
        }
    }
    assert!(checked > 10, "only checked {checked} endpoint paths");

    // The reverse: every v1 operation with a non-empty x-lmgw-endpoints
    // appears in every group it names.
    let mut op_checked = 0;
    for (path, methods) in paths {
        for (_, op) in methods.as_object().unwrap() {
            let Some(groups) = op.get(ext::ENDPOINTS).and_then(Value::as_array) else {
                continue;
            };
            for group in groups {
                let group = group.as_str().unwrap();
                let list = endpoints[group].as_array().unwrap_or_else(|| {
                    panic!("{path} names lmgw.endpoints group '{group}', which does not exist")
                });
                assert!(
                    list.iter().any(|p| p == path),
                    "{path} names group '{group}' but is not listed under it"
                );
                op_checked += 1;
            }
        }
    }
    assert!(op_checked > 10, "only checked {op_checked} operations");
}

// ---------------------------------------------------------------------------
// every_typed_extractor_is_referenced_by_the_doc
// ---------------------------------------------------------------------------

/// `text` with every `//...` line comment (including `///`/`//!` doc
/// comments) blanked out, respecting string literals — the same shape
/// `route_walk.rs`'s `close_paren` and `openapi_ops.rs`'s scanners give
/// themselves, so a `Query<T>` mentioned only in prose is not mistaken for a
/// real extractor.
fn strip_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
                out.push_str(&text[start..i]);
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            _ => {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    out
}

/// Every identifier named as `Query<X>` or `Json<X>` in `text` — single-level
/// generics only (every real extractor in this codebase is one), `X` itself
/// possibly module-qualified (`dto::Foo`). `Value` and `Args` (the op
/// dispatcher's raw-JSON extractors) are not doc-referenced types.
fn extractor_types(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for wrapper in ["Query<", "Json<"] {
        let mut from = 0;
        while let Some(at) = text[from..].find(wrapper) {
            let start = from + at + wrapper.len();
            let rest = &text[start..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
                .unwrap_or(rest.len());
            let name = &rest[..end];
            // Only a single-level generic: the character right after the
            // identifier must close the angle bracket.
            if rest[end..].starts_with('>') && !name.is_empty() && name != "Value" && name != "Args"
            {
                let last = name.rsplit("::").next().unwrap_or(name);
                out.insert(last.to_string());
            }
            from = start;
        }
    }
    out
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Whether `haystack` names `needle` as a generic type argument — a whole
/// word followed by `>` (`root_schema_for::<LogsQuery>()`,
/// `Vec<dto::JobRow>>`), which is how every schema is generated from a type.
/// Review R2 #7: a bare whole-word match let a type mentioned only in a
/// description string or a comment stand in for a schema that is never
/// generated (comments are stripped by the caller as well).
fn names_as_type_arg(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        if (start == 0 || !is_ident_byte(bytes[start - 1])) && bytes.get(end) == Some(&b'>') {
            return true;
        }
        from = start + 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[test]
fn names_as_type_arg_wants_a_generic_argument_in_code() {
    let src = strip_comments(
        "// LogsQuery is documented below\n\
         let d = \"see DocOnly for details\";\n\
         let s = g.root_schema_for::<LogsQuery>();\n\
         let v = g.root_schema_for::<Vec<dto::JobRow>>();\n",
    );
    assert!(names_as_type_arg(&src, "LogsQuery"));
    assert!(names_as_type_arg(&src, "JobRow"));
    assert!(!names_as_type_arg(&src, "DocOnly"));
    assert!(
        !names_as_type_arg(&src, "Query"),
        "a suffix of LogsQuery is not LogsQuery"
    );
    assert!(!names_as_type_arg(
        &strip_comments("// root_schema_for::<Ghost>()\n"),
        "Ghost"
    ));
}

#[test]
fn every_typed_extractor_is_referenced_by_the_doc() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");

    // The three internal mini-APIs (owner decision, 2026-09-28,
    // `openapi/exclusions.rs`) are deliberately undocumented — their own
    // extractor types can never appear under `src/openapi/**` and are not a
    // drift this guard should flag. The Chat's handlers live in `chat.rs`, its
    // `chat_*.rs` siblings (chat-complete design, ground rules: new logic in
    // new sibling modules) and the child modules named in `CHAT_DIRS`
    // (chat-voice's `chat_voice/`, the settings patch in `chat/`, the
    // ongoing folders' `current` in `chat_folders/`), so every one of those
    // counts. The
    // directories are named, not matched: a future non-Chat module under a
    // `chat_*` directory is scanned like any other.
    // The Knowledge page's backend (`api_knowledge.rs`, chat-complete §9.5)
    // is excluded the same way.
    let internal: BTreeSet<PathBuf> = ["audio_lab.rs", "image_lab.rs", "api_knowledge.rs"]
        .iter()
        .map(|f| src.join("web").join(f))
        .collect();
    const CHAT_DIRS: &[&str] = &["chat_voice", "chat", "chat_folders"];
    let web = src.join("web");
    let is_chat = |f: &PathBuf| {
        let Ok(rel) = f.strip_prefix(&web) else {
            return false;
        };
        let mut parts = rel.components().filter_map(|c| c.as_os_str().to_str());
        match (parts.next(), parts.next()) {
            (Some(file), None) => {
                file == "chat.rs" || (file.starts_with("chat_") && file.ends_with(".rs"))
            }
            (Some(dir), Some(_)) => CHAT_DIRS.contains(&dir),
            _ => false,
        }
    };

    let mut scan_files = Vec::new();
    collect_rs_files(&src.join("web"), &mut scan_files);
    scan_files.retain(|f| !internal.contains(f) && !is_chat(f));
    scan_files.push(src.join("server.rs"));
    scan_files.push(src.join("mcp").join("ingress.rs"));

    let mut types: BTreeSet<String> = BTreeSet::new();
    for file in &scan_files {
        let text = std::fs::read_to_string(file).unwrap();
        // A `#[cfg(test)]` tail builds its own fixtures, not this crate's
        // routes (route_walk.rs's own convention).
        let text = text.split("#[cfg(test)]").next().unwrap().to_string();
        types.extend(extractor_types(&strip_comments(&text)));
    }
    assert!(
        types.len() > 30,
        "only found {} extractor types",
        types.len()
    );

    let mut openapi_files = Vec::new();
    collect_rs_files(&src.join("openapi"), &mut openapi_files);
    let openapi_src: String = openapi_files
        .iter()
        .map(|f| strip_comments(&std::fs::read_to_string(f).unwrap()))
        .collect::<Vec<_>>()
        .join("\n");

    let missing: Vec<&String> = types
        .iter()
        .filter(|t| !names_as_type_arg(&openapi_src, t))
        .collect();
    assert!(
        missing.is_empty(),
        "these Query<>/Json<> extractor types are never generated from under src/openapi/** \
         (no `::<Type>`-style type argument outside a comment): {missing:#?}"
    );
}

// ---------------------------------------------------------------------------
// every_operation_has_a_summary
// ---------------------------------------------------------------------------

#[test]
fn every_operation_has_a_summary() {
    let doc = admin_doc();
    let mut checked = 0;
    for (path, methods) in doc["paths"].as_object().unwrap() {
        for (method, op) in methods.as_object().unwrap() {
            let summary = op["summary"].as_str().unwrap_or_default();
            assert!(!summary.trim().is_empty(), "{method} {path} has no summary");
            checked += 1;
        }
    }
    assert!(checked > 100, "only checked {checked} operations");
}

// ---------------------------------------------------------------------------
// every_example_validates_against_its_schema
// ---------------------------------------------------------------------------

/// `schema`, with `components` merged in at the top level so a `$ref` inside
/// it — including a bare top-level `{"$ref": ...}` schema — resolves against
/// `#/components/schemas/...` the way `jsonschema` resolves a JSON Pointer
/// fragment: against the root of the document being validated, not a
/// separately registered one (§7.1).
fn validator_root(schema: &Value, components: &Value) -> Value {
    let mut root = schema.clone();
    if let Value::Object(map) = &mut root {
        map.insert("components".to_string(), components.clone());
    }
    root
}

#[test]
fn every_example_validates_against_its_schema() {
    let doc = admin_doc();
    let components = doc["components"].clone();

    let mut checked = 0;
    for (path, methods) in doc["paths"].as_object().unwrap() {
        for (method, op) in methods.as_object().unwrap() {
            let Some(schema) = op.pointer("/requestBody/content/application~1json/schema") else {
                continue;
            };
            let Some(example) = schema.get("example") else {
                continue;
            };
            let root = validator_root(schema, &components);
            let result = jsonschema::validate(&root, example);
            assert!(
                result.is_ok(),
                "{method} {path}: example does not validate against its own schema: {:#?}\n\
                 example: {example:#?}",
                result.err()
            );
            checked += 1;
        }
    }
    assert!(checked > 5, "only checked {checked} examples");
}

/// Review W6-8: a `thread.*` / `folder.*` event's `oneOf` matches exactly one
/// branch — a row the row, a tombstone the tombstone — so a strict
/// validator, and a generated client that resolves `oneOf` by "exactly one
/// match", reads every event.
#[test]
fn a_feed_event_s_one_of_matches_exactly_one_branch() {
    let doc = admin_doc();
    let components = doc["components"].clone();
    let events = &doc["paths"]["/chat/api/feed"]["get"]["responses"]["200"][ext::SSE_EVENTS];
    let cases = [
        (
            "thread.updated",
            serde_json::json!({"id": 9, "title": "Plan", "by": "device 'phone'"}),
        ),
        (
            "thread.created",
            serde_json::json!({"thread_id": 9, "deleted": true, "by": null}),
        ),
        (
            "folder.updated",
            serde_json::json!({"id": 3, "name": "Desk", "by": null}),
        ),
        (
            "folder.created",
            serde_json::json!({"folder_id": 3, "deleted": true, "by": "the dashboard"}),
        ),
    ];
    for (event, instance) in cases {
        let schema = &events[event];
        assert!(schema.get("oneOf").is_some(), "{event}: {schema}");
        let root = validator_root(schema, &components);
        let result = jsonschema::validate(&root, &instance);
        assert!(
            result.is_ok(),
            "{event}: {instance} does not validate: {:#?}",
            result.err()
        );
    }
}
