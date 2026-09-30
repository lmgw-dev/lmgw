//! Per-operation tester state and the request it builds (api-docs design
//! §6.3, §6.6, §6.7, §6.9): the `Draft` signal bundle `page.rs` caches per
//! operation, path/query/header assembly, the request body's schema with its
//! `$ref`s resolved, what stops a send, the send itself, and the "Copy as
//! curl" text. The views over it are `tester.rs`.

use std::collections::{BTreeSet, HashMap};

use futures::StreamExt;
use leptos::prelude::*;
use serde_json::Value;

use lmgw_api_types::openapi_ext as ext;

use crate::scope::{Latest, Scope};

use super::curl;
use super::doc::{resolve_ref, Operation};
use super::example;
use super::identity::Identity;
use super::send::{
    self, header_problem, revoke_blob, Body as SendBody, MultipartPart as SendPart, Outcome,
    Prepared,
};
use super::stream::{assemble_text, SseFrame, SseSplitter};

const JSON: &str = "application/json";
const MULTIPART: &str = "multipart/form-data";

/// A finished request — or one whose SSE body is still arriving — as
/// `response_view.rs` renders it.
#[derive(Clone)]
pub struct Finished {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub headers_ms: f64,
    /// `None` while a stream is still open.
    pub done_ms: Option<f64>,
    pub outcome_kind: FinishedKind,
}

#[derive(Clone)]
pub enum FinishedKind {
    Json(Value),
    Text(String),
    /// No body at all (a `204`).
    Empty,
    /// The frames live in `Draft::sse_frames`; `error` says why the stream
    /// ended early (Stop, a dropped connection), if it did.
    Sse {
        error: Option<String>,
    },
    Blob {
        url: String,
        content_type: String,
        filename: String,
        size: usize,
    },
    Error(String),
}

/// Per-operation request/response state — a `Copy` bundle of signals, cached
/// by `page.rs` per `operation_id` so switching away and back keeps what was
/// typed (§6.3: `StoredValue<HashMap<String, Draft>>`).
///
/// Made under the *page's* owner (`page.rs` does `owner.with(..)`), never the
/// selection's re-rendering view: that view's owner is disposed on every op
/// switch, and a cached draft of disposed signals panics on the way back.
#[derive(Clone, Copy)]
pub struct Draft {
    pub path_params: RwSignal<HashMap<String, String>>,
    pub query_params: RwSignal<HashMap<String, String>>,
    pub header_params: RwSignal<HashMap<String, String>>,
    pub extra_headers: RwSignal<String>,
    pub body_text: RwSignal<String>,
    pub multipart_fields: RwSignal<HashMap<String, String>>,
    /// Picked files per multipart field (several for an array field), or
    /// the one raw body under `"file"`.
    pub files: RwSignal<HashMap<String, Vec<web_sys::File>>>,
    /// `Req::Raw`'s content-type field, defaulting to the picked file's own
    /// `type` (§6.6) — empty means "use the route's declared mime".
    pub raw_content_type: RwSignal<String>,
    pub in_flight: RwSignal<bool>,
    pub finished: RwSignal<Option<Finished>>,
    pub sse_frames: RwSignal<Vec<SseFrame>>,
    /// `assemble_text` over `sse_frames`, appended one batch at a time
    /// rather than re-parsed from every frame on each new one.
    pub sse_text: RwSignal<String>,
    pub show_curl: RwSignal<bool>,
    abort: StoredValue<Option<web_sys::AbortController>>,
    /// The page's scope: a send outlives a switch to another operation
    /// (§6.7), never the page.
    scope: Scope,
    /// Whose answer may still land: a Stop and a new Send must not have the
    /// stopped request's late answer overwrite the new one's state.
    latest: Latest,
}

impl Draft {
    /// Call under the page's owner (see the type's docs).
    pub fn new(op: &Operation, components: &Value, scope: Scope) -> Self {
        let mut query_params = HashMap::new();
        let mut header_params = HashMap::new();
        for p in op.parameters() {
            let Some(name) = p.get("name").and_then(Value::as_str) else {
                continue;
            };
            match p.get("in").and_then(Value::as_str) {
                Some("query") => {
                    query_params.insert(name.to_string(), String::new());
                }
                // `x-lmgw-prefill` (§4.4, §6.6): `anthropic-version` on the
                // Anthropic routes, `Accept` on `/mcp*`.
                Some("header") if flag(p, ext::PREFILL) => {
                    if let Some(v) = param_example(p) {
                        header_params.insert(name.to_string(), v);
                    }
                }
                _ => {}
            }
        }
        let body_text = op
            .request_body_schema()
            .filter(|(mime, _)| *mime == JSON)
            .map(|(_, schema)| {
                serde_json::to_string_pretty(&example::body_example(schema, components))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        // A multipart example's scalar values prefill the text fields (a
        // `model`, a `prompt`); file fields have nothing to prefill.
        let multipart_fields: HashMap<String, String> = op
            .request_body_schema()
            .filter(|(mime, _)| *mime == MULTIPART)
            .and_then(|(_, schema)| {
                schema
                    .get("example")
                    .or_else(|| resolved(schema, components).get("example"))
                    .and_then(Value::as_object)
                    .cloned()
            })
            .map(|ex| {
                ex.into_iter()
                    .filter_map(|(k, v)| match v {
                        Value::String(s) => Some((k, s)),
                        Value::Number(_) | Value::Bool(_) => Some((k, v.to_string())),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Draft {
            path_params: RwSignal::new(HashMap::new()),
            query_params: RwSignal::new(query_params),
            header_params: RwSignal::new(header_params),
            extra_headers: RwSignal::new(String::new()),
            body_text: RwSignal::new(body_text),
            multipart_fields: RwSignal::new(multipart_fields),
            files: RwSignal::new(HashMap::new()),
            raw_content_type: RwSignal::new(String::new()),
            in_flight: RwSignal::new(false),
            finished: RwSignal::new(None),
            sse_frames: RwSignal::new(Vec::new()),
            sse_text: RwSignal::new(String::new()),
            show_curl: RwSignal::new(false),
            abort: StoredValue::new(None),
            scope,
            latest: Latest::new(),
        }
    }

    /// Abort whatever is in flight — a Stop click, or the page cleanup
    /// aborting every draft at once (§6.7 "leaving the page aborts all").
    pub fn abort(&self) {
        if let Some(c) = self.abort.get_value() {
            c.abort();
        }
        self.abort.set_value(None);
        self.in_flight.set(false);
    }

    /// The Blob URL the current answer holds, if any — revoked on
    /// replacement and on page cleanup (§6.7).
    pub fn blob_url(&self) -> Option<String> {
        match self.finished.get_untracked()?.outcome_kind {
            FinishedKind::Blob { url, .. } => Some(url),
            _ => None,
        }
    }
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A parameter's example as editor text: its own `example`, else its
/// schema's `example`, `default`, or first `enum` value.
fn param_example(p: &Value) -> Option<String> {
    let schema = p.get("schema");
    let v = p
        .get("example")
        .or_else(|| schema.and_then(|s| s.get("example")))
        .or_else(|| schema.and_then(|s| s.get("default")))
        .or_else(|| {
            schema
                .and_then(|s| s.get("enum"))
                .and_then(Value::as_array)
                .and_then(|a| a.first())
        })?;
    Some(v.as_str().map_or_else(|| v.to_string(), str::to_string))
}

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

/// Follow `$ref`s until a schema that is not one (each name once, so a ref
/// cycle ends instead of looping).
pub fn resolved<'a>(schema: &'a Value, components: &'a Value) -> &'a Value {
    let mut cur = schema;
    let mut seen = BTreeSet::new();
    while let Some((name, next)) = resolve_ref(cur, components) {
        if !seen.insert(name) {
            break;
        }
        cur = next;
    }
    cur
}

/// The request body's `(mime, schema)` with a top-level `$ref` resolved —
/// `build.rs` hands most bodies over as a ref to a named component
/// (`schemas::named`), which has no `properties` of its own to read.
pub fn body_schema(op: &Operation, components: &Value) -> Option<(String, Value)> {
    let (mime, schema) = op.request_body_schema()?;
    Some((mime.to_string(), resolved(schema, components).clone()))
}

/// One multipart property (§6.6): a file input when it carries
/// `contentMediaType` (or is `format: binary`), `multiple` for an array.
#[derive(Clone, Debug, PartialEq)]
pub struct MultipartField {
    pub name: String,
    pub is_file: bool,
    pub multiple: bool,
}

pub fn multipart_fields(schema: &Value, components: &Value) -> Vec<MultipartField> {
    let binary = |s: &Value| {
        s.get("contentMediaType").is_some()
            || s.get("format").and_then(Value::as_str) == Some("binary")
    };
    resolved(schema, components)
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .map(|(name, prop)| {
                    let prop = resolved(prop, components);
                    let items = prop.get("items").map(|i| resolved(i, components));
                    MultipartField {
                        name: name.clone(),
                        is_file: binary(prop) || items.is_some_and(binary),
                        multiple: prop.get("type").and_then(Value::as_str) == Some("array"),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// §4.4's name-based `x-lmgw-secret` rule — the same list as `lmgw-core`'s
/// `openapi/build.rs` `SECRET_NAMES`. The doc marks every property of these
/// names (and, besides, the fields flagged where they are defined:
/// `extra_headers`, `forge_tokens`, an MCP server's `env`/`headers`, agent
/// config `values`); the tester redacts these names from a curl even where a
/// schema has not been marked — a copied curl ends up in chats and tickets.
const KNOWN_SECRETS: [&str; 4] = ["token", "api_key", "hf_token", "update_token"];

/// Every property name marked `x-lmgw-secret` anywhere in `schema`,
/// following refs (each once), plus [`KNOWN_SECRETS`].
pub fn secret_names(schema: &Value, components: &Value) -> Vec<String> {
    fn walk(
        v: &Value,
        components: &Value,
        seen: &mut BTreeSet<String>,
        out: &mut BTreeSet<String>,
    ) {
        match v {
            Value::Object(map) => {
                if let Some((name, target)) = resolve_ref(v, components) {
                    if seen.insert(name) {
                        walk(target, components, seen, out);
                    }
                }
                if let Some(props) = map.get("properties").and_then(Value::as_object) {
                    for (k, prop) in props {
                        if flag(prop, ext::SECRET) || flag(resolved(prop, components), ext::SECRET)
                        {
                            out.insert(k.clone());
                        }
                    }
                }
                for child in map.values() {
                    walk(child, components, seen, out);
                }
            }
            Value::Array(items) => {
                for child in items {
                    walk(child, components, seen, out);
                }
            }
            _ => {}
        }
    }
    let mut out: BTreeSet<String> = KNOWN_SECRETS.iter().map(|s| s.to_string()).collect();
    walk(schema, components, &mut BTreeSet::new(), &mut out);
    out.into_iter().collect()
}

/// Is this parameter a secret (itself, its schema, or a [`KNOWN_SECRETS`]
/// name)?
fn secret_param(p: &Value) -> bool {
    flag(p, ext::SECRET)
        || p.get("schema").is_some_and(|s| flag(s, ext::SECRET))
        || p.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| KNOWN_SECRETS.contains(&n))
}

fn params_in<'a>(op: &'a Operation, location: &'a str) -> impl Iterator<Item = &'a Value> {
    op.parameters()
        .iter()
        .filter(move |p| p.get("in").and_then(Value::as_str) == Some(location))
}

fn param_name(p: &Value) -> &str {
    p.get("name").and_then(Value::as_str).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Path, query, headers (§6.6, §7.4 "path filling with wildcard params")
// ---------------------------------------------------------------------------

/// RFC3986 unreserved characters only — deliberately stricter than a
/// browser's own `encodeURIComponent`; what matters here is that a `/` inside
/// a plain path value is escaped, and a `/` inside a wildcard value is not.
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Fill `{name}` path-template segments from `values`; a name in `wildcards`
/// is encoded segment by segment so `/` in its value survives (§6.6).
pub fn fill_path(op_path: &str, values: &HashMap<String, String>, wildcards: &[String]) -> String {
    op_path
        .split('/')
        .map(
            |seg| match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(name) => {
                    let value = values.get(name).cloned().unwrap_or_default();
                    if wildcards.iter().any(|w| w == name) {
                        value
                            .split('/')
                            .map(encode_component)
                            .collect::<Vec<_>>()
                            .join("/")
                    } else {
                        encode_component(&value)
                    }
                }
                None => seg.to_string(),
            },
        )
        .collect::<Vec<_>>()
        .join("/")
}

/// `?a=1&b=2`, non-empty values only, by name. A name `secret` says is one
/// prints as the literal `<secret: name>` (the curl text, §6.9).
fn build_query_string(values: &HashMap<String, String>, secret: &dyn Fn(&str) -> bool) -> String {
    let mut pairs: Vec<(&String, &String)> = values.iter().filter(|(_, v)| !v.is_empty()).collect();
    pairs.sort_by_key(|(k, _)| k.as_str());
    if pairs.is_empty() {
        return String::new();
    }
    let joined = pairs
        .iter()
        .map(|(k, v)| {
            let v = if secret(k) {
                curl::placeholder(k)
            } else {
                encode_component(v)
            };
            format!("{}={v}", encode_component(k))
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("?{joined}")
}

fn wildcard_names(op: &Operation) -> Vec<String> {
    params_in(op, "path")
        .filter(|p| flag(p, ext::WILDCARD))
        .map(|p| param_name(p).to_string())
        .collect()
}

/// `Name: value` lines; a line with no `:` is an error, not dropped.
fn extra_header_lines(text: &str) -> Result<Vec<(String, String)>, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| match line.split_once(':') {
            Some((name, value)) => Ok((name.trim().to_string(), value.trim().to_string())),
            // Not echoed: a pasted `Bearer …` line missing its name is
            // exactly the line that must not end up on screen in full.
            None => Err("an extra header line has no ':'".to_string()),
        })
        .collect()
}

/// One read of everything the draft holds — tracked when read inside a
/// reactive closure (the curl preview, the blocker chip), plain otherwise.
struct Values {
    path: HashMap<String, String>,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    extra: String,
    body: String,
    fields: HashMap<String, String>,
    files: HashMap<String, Vec<web_sys::File>>,
    raw_content_type: String,
}

impl Values {
    fn read(d: Draft) -> Self {
        Values {
            path: d.path_params.get(),
            query: d.query_params.get(),
            headers: d.header_params.get(),
            extra: d.extra_headers.get(),
            body: d.body_text.get(),
            fields: d.multipart_fields.get(),
            files: d.files.get(),
            raw_content_type: d.raw_content_type.get(),
        }
    }

    fn header_list(&self) -> Result<Vec<(String, String)>, String> {
        let mut out: Vec<(String, String)> = self
            .headers
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort();
        out.extend(extra_header_lines(&self.extra)?);
        Ok(out)
    }

    fn path(&self, op: &Operation, secret: &dyn Fn(&str) -> bool) -> String {
        let path = fill_path(&op.path, &self.path, &wildcard_names(op));
        format!("{path}{}", build_query_string(&self.query, secret))
    }

    fn json_body(&self) -> Option<Value> {
        serde_json::from_str(self.body.trim()).ok()
    }

    /// `(field, value)` text parts and `(field, files)` file parts, in the
    /// schema's property order.
    fn multipart(&self, schema: &Value, components: &Value) -> Vec<SendPart> {
        let mut parts = Vec::new();
        for f in multipart_fields(schema, components) {
            match self.files.get(&f.name) {
                Some(files) if f.is_file => {
                    for file in files {
                        parts.push(SendPart::File(f.name.clone(), file.clone()));
                    }
                }
                _ => {
                    if let Some(v) = self.fields.get(&f.name).filter(|v| !v.is_empty()) {
                        parts.push(SendPart::Field(f.name.clone(), v.clone()));
                    }
                }
            }
        }
        parts
    }

    fn raw_file(&self) -> Option<web_sys::File> {
        self.files.get("file").and_then(|f| f.first()).cloned()
    }
}

/// Why this request cannot go out as it stands (§6.6 "must parse before
/// sending", a required path segment left empty, a header the browser would
/// throw on) — `None` when it can. Tracked.
pub fn blocker(op: &Operation, draft: Draft, identity: Identity, key: &str) -> Option<String> {
    let v = Values::read(draft);
    for p in params_in(op, "path") {
        let name = param_name(p);
        if v.path.get(name).is_none_or(|s| s.trim().is_empty()) {
            return Some(format!("fill in the path parameter '{name}'"));
        }
    }
    if op
        .request_body_schema()
        .is_some_and(|(mime, _)| mime == JSON)
    {
        if v.body.trim().is_empty() {
            if op.request_body_required() {
                return Some("the request body is empty".to_string());
            }
        } else if v.json_body().is_none() {
            return Some("the body is not valid JSON".to_string());
        }
    }
    let headers = match v.header_list() {
        Ok(h) => h,
        Err(e) => return Some(e),
    };
    if let Some(problem) = headers.iter().find_map(|(n, val)| header_problem(n, val)) {
        return Some(problem);
    }
    if let Some(problem) = header_problem("Content-Type", v.raw_content_type.trim())
        .filter(|_| !v.raw_content_type.trim().is_empty())
    {
        return Some(problem);
    }
    if identity == Identity::Key
        && header_problem("Authorization", &format!("Bearer {key}")).is_some()
    {
        return Some("the pasted key has characters a header cannot carry".to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// Sending (§6.7)
// ---------------------------------------------------------------------------

fn js_error_text(e: &wasm_bindgen::JsValue) -> String {
    e.as_string()
        .or_else(|| {
            js_sys::Reflect::get(e, &"message".into())
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| "the stream ended with an error".to_string())
}

/// Send what the draft holds. Never through `crate::api` (§6.7): see
/// `send.rs`. The task runs in the page's [`Scope`] and its answer lands
/// only while it is still this draft's latest send.
pub fn send_request(
    op: &Operation,
    draft: Draft,
    components: &Value,
    identity: Identity,
    key: String,
) {
    if draft.in_flight.get_untracked() {
        return;
    }
    let prepared = untrack(|| {
        if blocker(op, draft, identity, &key).is_some() {
            return None;
        }
        let v = Values::read(draft);
        let body = match body_schema(op, components) {
            Some((mime, _)) if mime == JSON => {
                if v.body.trim().is_empty() {
                    SendBody::None
                } else {
                    SendBody::Json {
                        text: v.body.clone(),
                    }
                }
            }
            Some((mime, schema)) if mime == MULTIPART => SendBody::Multipart {
                parts: v.multipart(&schema, components),
            },
            Some((mime, _)) => match v.raw_file() {
                Some(file) => SendBody::Raw {
                    file,
                    content_type: if v.raw_content_type.trim().is_empty() {
                        mime
                    } else {
                        v.raw_content_type.trim().to_string()
                    },
                },
                None => SendBody::None,
            },
            None => SendBody::None,
        };
        Some(Prepared {
            method: op.method.clone(),
            path: v.path(op, &|_| false),
            headers: v.header_list().ok()?,
            identity,
            key,
            body,
        })
    });
    let Some(prepared) = prepared else { return };
    let Some(ticket) = draft.latest.next() else {
        return;
    };

    // The last answer is replaced: its Blob URL goes with it (§6.7).
    if let Some(url) = draft.blob_url() {
        revoke_blob(&url);
    }
    draft.finished.set(None);
    draft.sse_frames.set(Vec::new());
    draft.sse_text.set(String::new());
    draft.in_flight.set(true);
    let controller = send::new_abort();
    let signal = controller.signal();
    draft.abort.set_value(Some(controller));
    let fallback_name = format!("{}.bin", op.operation_id);

    draft.scope.spawn(async move {
        let t0 = send::now_ms();
        let answer = send::send(&prepared, &signal, &fallback_name).await;
        let headers_ms = send::now_ms() - t0;
        if !draft.latest.is(ticket) {
            // A newer send took over while this one was out.
            if let Outcome::Blob { url, .. } = &answer.outcome {
                revoke_blob(url);
            }
            return;
        }
        let finished = |kind, done_ms| Finished {
            status: answer.status,
            headers: answer.headers.clone(),
            headers_ms,
            done_ms,
            outcome_kind: kind,
        };
        match answer.outcome {
            Outcome::Sse(raw) => {
                // Headers are in: show them now, then grow `sse_frames` as
                // the body streams (§6.7) — the same ReadableStream → chunk
                // loop `pages::chat_stream` uses, over the generic splitter.
                draft
                    .finished
                    .set(Some(finished(FinishedKind::Sse { error: None }, None)));
                let mut reader = wasm_streams::ReadableStream::from_raw(raw).into_stream();
                let mut splitter = SseSplitter::new();
                let mut error = None;
                while let Some(chunk) = reader.next().await {
                    if !draft.latest.is(ticket) {
                        return;
                    }
                    let chunk = match chunk {
                        Ok(c) => c,
                        Err(e) => {
                            error = Some(js_error_text(&e));
                            break;
                        }
                    };
                    let frames = splitter.push(&js_sys::Uint8Array::from(chunk).to_vec());
                    if frames.is_empty() {
                        continue;
                    }
                    let text = assemble_text(&frames);
                    draft.sse_frames.update(|f| f.extend(frames));
                    if !text.is_empty() {
                        draft.sse_text.update(|t| t.push_str(&text));
                    }
                }
                if !draft.latest.is(ticket) {
                    return;
                }
                let done = send::now_ms() - t0;
                draft.finished.update(|f| {
                    if let Some(f) = f {
                        f.done_ms = Some(done);
                        f.outcome_kind = FinishedKind::Sse { error };
                    }
                });
            }
            other => {
                let kind = match other {
                    Outcome::Json(v) => FinishedKind::Json(v),
                    Outcome::Text(t) => FinishedKind::Text(t),
                    Outcome::Empty => FinishedKind::Empty,
                    Outcome::Blob {
                        url,
                        content_type,
                        filename,
                        size,
                    } => FinishedKind::Blob {
                        url,
                        content_type,
                        filename,
                        size,
                    },
                    Outcome::TransportError(e) => FinishedKind::Error(e),
                    Outcome::Sse(_) => unreachable!("matched above"),
                };
                let done = Some(send::now_ms() - t0);
                draft.finished.set(Some(finished(kind, done)));
            }
        }
        draft.in_flight.set(false);
        draft.abort.set_value(None);
    });
}

// ---------------------------------------------------------------------------
// Copy as curl (§6.9)
// ---------------------------------------------------------------------------

/// The curl text for what the draft holds now. Tracked, so the preview
/// follows the editors. The pasted key never reaches it: `curl::Prepared`
/// has no field that could carry it.
pub fn curl_command_for(
    op: &Operation,
    draft: Draft,
    components: &Value,
    identity: Identity,
    origin: &str,
) -> String {
    let v = Values::read(draft);
    let query_secrets: Vec<String> = params_in(op, "query")
        .filter(|p| secret_param(p))
        .map(|p| param_name(p).to_string())
        .collect();
    let header_secrets: Vec<String> = params_in(op, "header")
        .filter(|p| secret_param(p))
        .map(|p| param_name(p).to_ascii_lowercase())
        .collect();
    let headers = v
        .header_list()
        .unwrap_or_default()
        .into_iter()
        .map(|(n, val)| {
            if header_secrets.contains(&n.to_ascii_lowercase()) {
                let p = curl::placeholder(&n);
                (n, p)
            } else {
                (n, val)
            }
        })
        .collect();
    let body = match body_schema(op, components) {
        Some((mime, schema)) if mime == JSON => {
            if v.body.trim().is_empty() {
                curl::Body::None
            } else {
                curl::Body::Json {
                    value: v.json_body().unwrap_or(Value::Null),
                    secret_props: secret_names(&schema, components),
                }
            }
        }
        Some((mime, schema)) if mime == MULTIPART => curl::Body::Multipart {
            parts: v
                .multipart(&schema, components)
                .into_iter()
                .map(|p| match p {
                    SendPart::Field(n, val) => curl::MultipartPart::Field(n, val),
                    SendPart::File(n, f) => curl::MultipartPart::File(n, f.name()),
                })
                .collect(),
            secret_props: secret_names(&schema, components),
        },
        Some((mime, _)) => curl::Body::Raw {
            file_name: v.raw_file().map(|f| f.name()).unwrap_or_default(),
            content_type: if v.raw_content_type.trim().is_empty() {
                mime
            } else {
                v.raw_content_type.trim().to_string()
            },
        },
        None => curl::Body::None,
    };
    let stream = op.is_sse()
        || v.json_body()
            .and_then(|b| b.get("stream").and_then(Value::as_bool))
            .unwrap_or(false);
    let prepared = curl::Prepared {
        method: op.method.clone(),
        path: v.path(op, &|name| query_secrets.iter().any(|s| s == name)),
        headers,
        identity,
        stream,
        body,
    };
    curl::curl_command(&prepared, origin)
}

// ---------------------------------------------------------------------------
// The model field (§6.6)
// ---------------------------------------------------------------------------

/// Where the `ModelPicker`'s choice lives for this operation: the JSON
/// body's top-level `model`, the multipart `model` field, or the `model`
/// query parameter of a GET.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelSlot {
    Json,
    Multipart,
    Query,
}

impl ModelSlot {
    pub fn for_op(op: &Operation, components: &Value) -> Option<Self> {
        op.model_task.as_ref()?;
        match body_schema(op, components) {
            Some((mime, _)) if mime == JSON => Some(Self::Json),
            Some((mime, schema)) if mime == MULTIPART => multipart_fields(&schema, components)
                .iter()
                .any(|f| f.name == "model")
                .then_some(Self::Multipart),
            Some(_) => None,
            None => params_in(op, "query")
                .any(|p| param_name(p) == "model")
                .then_some(Self::Query),
        }
    }

    /// The model the slot holds now, if any. Tracked.
    pub fn read(self, d: Draft) -> Option<String> {
        let non_empty = |v: Option<String>| v.filter(|s| !s.is_empty());
        match self {
            Self::Json => d.body_text.with(|t| example::get_model(t)),
            Self::Multipart => non_empty(d.multipart_fields.with(|m| m.get("model").cloned())),
            Self::Query => non_empty(d.query_params.with(|m| m.get("model").cloned())),
        }
    }

    /// Write `model` into the slot; a JSON text that does not parse as an
    /// object is left alone (the picker is disabled then).
    pub fn write(self, d: Draft, model: &str) {
        match self {
            Self::Json => {
                if let Some(updated) = example::set_model(&d.body_text.get_untracked(), model) {
                    d.body_text.set(updated);
                }
            }
            Self::Multipart => d.multipart_fields.update(|m| {
                m.insert("model".to_string(), model.to_string());
            }),
            Self::Query => d.query_params.update(|m| {
                m.insert("model".to_string(), model.to_string());
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_wildcard_path_param_keeps_its_slashes() {
        let mut values = HashMap::new();
        values.insert("id".to_string(), "chat/thread one".to_string());
        let filled = fill_path("/v1/responses/{id}", &values, &["id".to_string()]);
        assert_eq!(filled, "/v1/responses/chat/thread%20one");
    }

    #[test]
    fn a_plain_path_param_is_fully_encoded() {
        let mut values = HashMap::new();
        values.insert("id".to_string(), "a/b".to_string());
        let filled = fill_path("/api/agents/{id}", &values, &[]);
        assert_eq!(filled, "/api/agents/a%2Fb");
    }

    #[test]
    fn literal_segments_are_untouched() {
        let values = HashMap::new();
        assert_eq!(
            fill_path("/v1/chat/completions", &values, &[]),
            "/v1/chat/completions"
        );
    }

    #[test]
    fn query_string_only_includes_non_empty_values_sorted_by_name() {
        let mut values = HashMap::new();
        values.insert("b".to_string(), "2 3".to_string());
        values.insert("a".to_string(), "1".to_string());
        values.insert("c".to_string(), String::new());
        assert_eq!(build_query_string(&values, &|_| false), "?a=1&b=2%203");
        assert_eq!(build_query_string(&HashMap::new(), &|_| false), "");
    }

    #[test]
    fn a_secret_query_value_prints_as_its_placeholder() {
        let mut values = HashMap::new();
        values.insert("hf_token".to_string(), "hf_real".to_string());
        values.insert("q".to_string(), "x".to_string());
        let qs = build_query_string(&values, &|n| n == "hf_token");
        assert_eq!(qs, "?hf_token=<secret: hf_token>&q=x");
    }

    #[test]
    fn extra_header_lines_refuse_a_line_without_a_colon() {
        assert_eq!(
            extra_header_lines("x-a: 1\n\n  x-b:2  \n").unwrap(),
            vec![
                ("x-a".to_string(), "1".to_string()),
                ("x-b".to_string(), "2".to_string())
            ]
        );
        assert!(extra_header_lines("x-a 1").is_err());
    }

    fn components() -> Value {
        json!({"schemas": {
            "Transcription": {
                "type": "object",
                "properties": {
                    "file": {"type": "string", "contentMediaType": "audio/*"},
                    "images": {"type": "array", "items": {"$ref": "#/components/schemas/Img"}},
                    "model": {"type": "string"},
                },
            },
            "Img": {"type": "string", "format": "binary"},
            "Upstream": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "api_key": {"type": "string", "x-lmgw-secret": true},
                    "patch": {"$ref": "#/components/schemas/Patch"},
                },
            },
            "Patch": {
                "type": "object",
                "properties": {"client_secret": {"type": "string", "x-lmgw-secret": true}},
            },
            "Loop": {"$ref": "#/components/schemas/Loop"},
        }})
    }

    #[test]
    fn multipart_fields_read_through_refs_and_arrays() {
        let c = components();
        let fields = multipart_fields(&json!({"$ref": "#/components/schemas/Transcription"}), &c);
        assert_eq!(
            fields,
            vec![
                MultipartField {
                    name: "file".into(),
                    is_file: true,
                    multiple: false
                },
                MultipartField {
                    name: "images".into(),
                    is_file: true,
                    multiple: true
                },
                MultipartField {
                    name: "model".into(),
                    is_file: false,
                    multiple: false
                },
            ]
        );
    }

    #[test]
    fn secret_names_follow_refs_into_nested_objects() {
        let c = components();
        let names = secret_names(&json!({"$ref": "#/components/schemas/Upstream"}), &c);
        assert!(names.contains(&"api_key".to_string()));
        assert!(names.contains(&"client_secret".to_string()));
        assert!(!names.contains(&"name".to_string()));
        // The §4.4 names are redacted even where nothing is marked.
        assert!(secret_names(&json!({}), &c).contains(&"hf_token".to_string()));
    }

    #[test]
    fn a_ref_cycle_resolves_to_itself_instead_of_looping() {
        let c = components();
        let r = json!({"$ref": "#/components/schemas/Loop"});
        assert!(resolved(&r, &c).get("$ref").is_some());
        let _ = secret_names(&r, &c);
    }

    #[test]
    fn a_prefill_header_takes_its_example() {
        let p = json!({"name": "anthropic-version", "in": "header",
            "x-lmgw-prefill": true, "schema": {"type": "string", "example": "2023-06-01"}});
        assert_eq!(param_example(&p), Some("2023-06-01".to_string()));
        let p = json!({"name": "Accept", "in": "header", "example": "application/json, text/event-stream"});
        assert_eq!(
            param_example(&p),
            Some("application/json, text/event-stream".to_string())
        );
    }
}
