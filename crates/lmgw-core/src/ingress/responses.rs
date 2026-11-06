//! OpenAI **Responses API** ingress (§21): `POST /v1/responses`.
//!
//! Unlike the other two ingresses this is not a 1:1 translation — a Responses
//! request can produce several upstream turns with tool calls in between, which
//! is what [`crate::agent`] drives. This module owns only the wire format:
//! parsing `input` items into the IR, and turning [`LoopEvent`]s into the
//! `output` array and the `response.*` SSE sequence.
//!
//! **Why lmgw implements this at all.** llama-server has no `/v1/responses`
//! (ggml-org/llama.cpp#19138), so a local model cannot serve Responses clients;
//! and even the shim discussed there would forward to chat/completions and so
//! could not execute tools server-side. lmgw already runs the MCP servers, so
//! the interesting half — OpenAI's `{"type": "mcp"}` tool, where the *server*
//! lists and calls the tools — is the half it is best placed to provide.
//!
//! ## What is faithful, and what is deliberately refused
//!
//! Faithful: input items (messages, `function_call`, `function_call_output`,
//! `mcp_call`), the `output` item shapes, the streaming event names and their
//! `sequence_number`/`output_index` discipline, `usage` in Responses' own
//! nesting, `status: completed | incomplete`, and `incomplete_details.reason`.
//!
//! Accepted here, run only natively ([`native_only`], openai egress design
//! §6.1): **hosted tools** (`web_search`, `file_search`, `code_interpreter`,
//! `image_generation`, `computer_use_preview`, …), a `tool_choice` naming
//! one, input items and content parts the loop has no reading for
//! (`item_reference`, a hosted tool's `*_call`, `input_file`, an image by
//! `file_id`), and **`truncation: "auto"`**. A request forwarded natively
//! takes them to the upstream as sent; the loop refuses them, loudly, rather
//! than silently approximating them (§14), with the error this parser used
//! to answer at once.
//!
//! Refused here, on every path:
//!
//! - (`previous_response_id` is not refused here: this stage only parses it.
//!   `crate::responses` stores responses and continues from one. The response
//!   object's `store` is overridden with what the gateway actually did, so a
//!   client can see whether a chained call will work before it tries.)
//! - **`background: true`** — a background response is answered at once and
//!   finished later, out of the relay's sight, so its usage would never reach
//!   a row: a native one would go unmetered, past every budget.
//! - **`require_approval` other than `"never"`** — auto-approving a call the
//!   client asked to gate would be a security hole, so it is an error until the
//!   approval round trip exists.

use serde_json::{json, Map, Value};

use crate::agent::{LoopEvent, StopReason};
use crate::candidates::Facet;
use crate::egress::openai_wire::responses_items::{event, item, part};
use crate::error::GatewayError;
use crate::ingress::{now_unix, rand_id};
use crate::ir::structured::{schema_fields, DEFAULT_SCHEMA_NAME};
use crate::ir::ResponseFormat;
use crate::ir::{
    flatten_tool_result, ChatRequest, ContentPart, FinishReason, Message, Params, ReasoningControl,
    Role, ToolChoice, ToolDef, ToolResultBlock, Usage,
};
use crate::mcp::spec::kind_of;
use crate::sse::frame;

#[cfg(test)]
mod fail_tests;
mod native_only;
mod tool_output;
pub use native_only::{NativeOnly, NativeOnlyParts};
pub(crate) use tool_output::url_image_resource;

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// The `mcp` tool entry, parsed by the code `/v1/realtime` shares
/// (realtime-server-tools design §1.1).
pub use crate::mcp::spec::{ApprovalRule, McpToolSpec};

/// One `mcp_approval_response` input item: the client's verdict on a call an
/// earlier response stopped at.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalDecision {
    pub approval_request_id: String,
    pub approve: bool,
    /// Optional client-supplied reason; fed to the model with the refusal so it
    /// learns *why*, rather than just that something failed.
    pub reason: Option<String>,
}

/// A parsed `/v1/responses` request.
#[derive(Debug, Clone)]
pub struct ResponsesRequest {
    pub ir: ChatRequest,
    /// Client-declared `function` tools — the loop stops and hands these back.
    pub client_tools: Vec<ToolDef>,
    /// `{"type": "mcp"}` entries — the loop executes these.
    pub mcp_tools: Vec<McpToolSpec>,
    pub stream: bool,
    /// `max_tool_calls`, when the client bounded the run itself.
    pub max_tool_calls: Option<u32>,
    pub parallel_tool_calls: bool,
    /// Echoed back verbatim on the response object.
    pub metadata: Value,
    /// `instructions`, echoed back on the response object as OpenAI does.
    pub instructions: Option<String>,
    /// `store` as the client asked for it; `None` means "unspecified", which the
    /// API defaults to `true`. The gateway's own setting can still refuse.
    pub store: Option<bool>,
    /// The stored response this call continues.
    pub previous_response_id: Option<String>,
    /// `mcp_approval_response` items — verdicts on the previous response's
    /// pending calls, not conversation turns.
    pub approvals: Vec<ApprovalDecision>,
    /// This turn's `input` array as sent, for `GET /v1/responses/{id}/input_items`.
    pub input_items: Value,
    /// What only an upstream's own `/v1/responses` can serve: accepted
    /// here, refused on the loop path ([`native_only`]).
    pub native_only: NativeOnlyParts,
}

pub fn parse_request(body: &Value) -> Result<ResponsesRequest, GatewayError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::BadRequest("missing 'model'".into()))?
        .to_string();

    reject_unsupported(body)?;
    let mut native_only = NativeOnlyParts::default();
    if body.get("truncation").and_then(Value::as_str) == Some("auto") {
        native_only.note(
            "truncation \"auto\"".into(),
            GatewayError::Unsupported(
                "'truncation': \"auto\" — this gateway never silently drops \
                 conversation history; send input that fits the model's context"
                    .into(),
            ),
        );
    }

    let previous_response_id = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let mut messages = Vec::new();
    // Kept as `Some("")` when the client sent an empty string rather than
    // collapsing it to `None`: on a chained call that is how a client says
    // "drop the system prompt", which is a different instruction from "say
    // nothing about it".
    let instructions = body
        .get("instructions")
        .and_then(Value::as_str)
        .map(String::from);
    if let Some(text) = instructions.as_ref().filter(|s| !s.is_empty()) {
        messages.push(Message::text(Role::System, text.clone()));
    }
    let (turns, approvals) = parse_input(body.get("input"), &mut native_only)?;
    messages.extend(turns);
    // A continued call may legitimately carry nothing but approval verdicts —
    // the conversation it resumes lives in the stored response. Input only a
    // native upstream reads (item references, say) is a conversation too:
    // the loop refuses it by name.
    let empty = messages.iter().all(|m| m.role == Role::System);
    if empty && approvals.is_empty() && previous_response_id.is_none() && native_only.is_empty() {
        return Err(GatewayError::BadRequest(
            "'input' produced no conversation turns".into(),
        ));
    }

    let (client_tools, mcp_tools) = parse_tools(body.get("tools"), &mut native_only)?;
    let tool_choice = parse_tool_choice(body.get("tool_choice"), &mut native_only)?;

    let params = Params {
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        top_k: body.get("top_k").and_then(Value::as_u64).map(|v| v as u32),
        max_tokens: body
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        min_p: None,
        repeat_penalty: None,
        presence_penalty: None,
        frequency_penalty: None,
        seed: None,
        stop: Vec::new(),
        // `reasoning` has been in MODELED_KEYS since this parser was written —
        // kept out of passthrough, and then never read. This is the read.
        reasoning: parse_reasoning(body),
        // A send's own, set by what the provider taught it.
        reasoning_summary_refused: false,
        reasoning_summary: parse_summary(body),
    };

    Ok(ResponsesRequest {
        ir: ChatRequest {
            model_alias: model,
            messages,
            params,
            tools: Vec::new(), // filled by the loop from the resolved set
            tool_choice,
            response_format: parse_text_format(body)?,
            stream: false,
            passthrough: with_verbosity(passthrough_fields(body), body),
            // No template kwargs in this dialect.
            llama_kwargs_enabled: None,
            // A header, not a body field: the handler fills it from the request.
            anthropic_beta: Vec::new(),
            context_management: None,
            compaction: None,
            thinking_display: None,
        },
        client_tools,
        mcp_tools,
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        max_tool_calls: body
            .get("max_tool_calls")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        parallel_tool_calls: body
            .get("parallel_tool_calls")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        metadata: body.get("metadata").cloned().unwrap_or(Value::Null),
        instructions,
        store: body.get("store").and_then(Value::as_bool),
        previous_response_id,
        approvals,
        input_items: match body.get("input") {
            Some(Value::Array(a)) => Value::Array(a.clone()),
            Some(Value::String(s)) => json!([{
                "type": item::MESSAGE, "role": "user",
                "content": [{"type": part::INPUT_TEXT, "text": s}],
            }]),
            _ => Value::Array(vec![]),
        },
        native_only,
    })
}

/// The Responses dialect's one reasoning control: `reasoning.effort` (§5.2/§5.5).
/// `summary` / `generate_summary` are about *reporting* the trace, not about
/// producing it, and stay the encoder's business.
fn parse_reasoning(body: &Value) -> Option<ReasoningControl> {
    let effort = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(String::from)?;
    Some(ReasoningControl {
        effort: Some(effort),
        ..Default::default()
    })
}

/// `reasoning.summary` (or its deprecated `generate_summary`) as the IR
/// carries it ([`Params::reasoning_summary`]): absent is `None`, `null` is
/// no summary, a kind is that kind.
fn parse_summary(body: &Value) -> Option<Option<String>> {
    let r = body.get("reasoning")?;
    let v = r.get("summary").or_else(|| r.get("generate_summary"))?;
    Some(v.as_str().map(String::from))
}

/// `text.verbosity` into the IR as the chat dialect spells it, the
/// passthrough key `verbosity`: Chat Completions takes it there, and the
/// platform's Responses wire writes it back as `text.verbosity`.
fn with_verbosity(mut passthrough: Map<String, Value>, body: &Value) -> Map<String, Value> {
    if let Some(v) = body.pointer("/text/verbosity").filter(|v| !v.is_null()) {
        passthrough.insert("verbosity".into(), v.clone());
    }
    passthrough
}

/// Fields refused on every path. `background: true` is one even on an
/// upstream that runs it natively: its answer arrives at once with no usage
/// and the response finishes later, out of the relay's sight, so no row
/// would ever carry what it spent and no budget would see it (openai egress
/// design §6.1).
fn reject_unsupported(body: &Value) -> Result<(), GatewayError> {
    if body.get("background").and_then(Value::as_bool) == Some(true) {
        return Err(GatewayError::Unsupported(
            "'background': responses are always run synchronously here — a background \
             response finishes after its answer has left, so lmgw could never record what \
             it spent or hold it to a budget; send it without 'background' (and with \
             'stream' for a long run)"
                .into(),
        ));
    }
    Ok(())
}

/// Request keys this parser consumes. Anything else rides along in
/// `passthrough` so an OpenAI-compatible upstream still receives it, the same
/// bargain the chat ingress strikes.
const MODELED_KEYS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "stream",
    "store",
    "metadata",
    "temperature",
    "top_p",
    "top_k",
    "max_output_tokens",
    "max_tool_calls",
    "parallel_tool_calls",
    "previous_response_id",
    "background",
    "truncation",
    "include",
    "reasoning",
    "text",
];

/// `text` is modelled: its `format` is the IR's structured output
/// ([`parse_text_format`]).
fn passthrough_fields(body: &Value) -> Map<String, Value> {
    match body.as_object() {
        Some(obj) => obj
            .iter()
            .filter(|(k, _)| !MODELED_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        None => Map::new(),
    }
}

/// `text.format` as the IR's structured output (structured-output design §5).
/// Responses states the format flat — `{type, name, description, schema,
/// strict}` — where chat nests it under `json_schema`; same feature, so it is
/// translated, with the defaults this route always used (`name` `response`,
/// `strict` false, an empty schema). A type lmgw does not model is kept
/// verbatim: a native Responses upstream gets the body as it came, and the
/// synthesized loop refuses it (`crate::responses`), since no chat spelling
/// exists to send it as.
fn parse_text_format(body: &Value) -> Result<Option<ResponseFormat>, GatewayError> {
    let fmt = match body.pointer("/text/format") {
        None | Some(Value::Null) => return Ok(None),
        Some(f) => f,
    };
    let o = fmt
        .as_object()
        .ok_or_else(|| GatewayError::BadRequest("'text.format' must be an object".into()))?;
    let t = o
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::BadRequest("'text.format' has no string 'type'".into()))?;
    Ok(Some(match t {
        "text" => ResponseFormat::text(),
        "json_object" => ResponseFormat::JsonObject {
            schema: None,
            extra: Default::default(),
        },
        "json_schema" => {
            let mut f = schema_fields(o, "text.format")?;
            f.name
                .get_or_insert_with(|| DEFAULT_SCHEMA_NAME.to_string());
            f.schema.get_or_insert_with(|| json!({}));
            f.strict.get_or_insert(false);
            ResponseFormat::JsonSchema(f)
        }
        _ => ResponseFormat::Other { raw: fmt.clone() },
    }))
}

/// `tool_choice` as the IR's. A choice naming a tool type the loop does not
/// run (a hosted tool, `allowed_tools`, an `mcp` server) has no IR form: it
/// is noted as [`native_only`] and the IR's choice is left unset, since only
/// the native path, which forwards the body as sent, can serve it.
fn parse_tool_choice(
    v: Option<&Value>,
    native_only: &mut NativeOnlyParts,
) -> Result<Option<ToolChoice>, GatewayError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => match s.as_str() {
            "auto" => Ok(Some(ToolChoice::Auto)),
            "none" => Ok(Some(ToolChoice::None)),
            "required" => Ok(Some(ToolChoice::Required)),
            other => Err(GatewayError::BadRequest(format!(
                "unknown tool_choice '{other}'"
            ))),
        },
        Some(v) => match v.get("type").and_then(Value::as_str) {
            Some("function") => Ok(v
                .get("name")
                .and_then(Value::as_str)
                .map(|name| ToolChoice::Tool { name: name.into() })),
            other => {
                let e = GatewayError::Unsupported(format!(
                    "tool_choice type {other:?} — only \"auto\"/\"none\"/\"required\" \
                     and {{\"type\":\"function\",\"name\":…}} are translatable"
                ));
                match other {
                    Some(t) => {
                        native_only.note_using(
                            format!("tool_choice type '{t}'"),
                            Some(Facet::ToolCalls),
                            e,
                        );
                        Ok(None)
                    }
                    // No type at all is no choice any upstream reads.
                    None => Err(e),
                }
            }
        },
    }
}

/// The `function` and `mcp` tools the loop runs. Any other type is a tool
/// the upstream hosts (`web_search`, `file_search`, …), noted as
/// [`native_only`]: the native path forwards it, the loop refuses it.
fn parse_tools(
    v: Option<&Value>,
    native_only: &mut NativeOnlyParts,
) -> Result<(Vec<ToolDef>, Vec<McpToolSpec>), GatewayError> {
    let mut functions = Vec::new();
    let mut mcp = Vec::new();
    let Some(arr) = v.and_then(Value::as_array) else {
        return Ok((functions, mcp));
    };
    for t in arr {
        match t.get("type").and_then(Value::as_str) {
            Some("function") => functions.push(ToolDef {
                name: t
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| GatewayError::BadRequest("function tool without name".into()))?
                    .to_string(),
                description: t
                    .get("description")
                    .and_then(Value::as_str)
                    .map(String::from),
                parameters: t
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                // None where the client said none: a Responses tool is then
                // strict when its schema allows it (ToolDef::strict).
                strict: t.get("strict").and_then(Value::as_bool),
            }),
            Some("mcp") => {
                mcp.push(crate::mcp::spec::parse_mcp_tool(t).map_err(GatewayError::BadRequest)?)
            }
            other => {
                let e = GatewayError::Unsupported(format!(
                    "tool type {other:?} — this gateway runs `function` and `mcp` tools; \
                     hosted tools (web_search, file_search, code_interpreter, \
                     image_generation, computer_use) have no local implementation"
                ));
                match other {
                    Some(t) => native_only.note_using(
                        format!("tool type '{t}'"),
                        Some(Facet::ToolCalls),
                        e,
                    ),
                    // A tool without a type is malformed on any upstream.
                    None => return Err(e),
                }
            }
        }
    }
    Ok((functions, mcp))
}

/// `input` → IR messages plus any approval verdicts. Accepts the plain-string
/// form and the item array, including the `function_call` /
/// `function_call_output` / `mcp_call` items this gateway itself emits — without
/// those a stateless client could not replay a conversation that had already
/// used tools.
type ParsedInput = (Vec<Message>, Vec<ApprovalDecision>);

fn parse_input(
    v: Option<&Value>,
    native_only: &mut NativeOnlyParts,
) -> Result<ParsedInput, GatewayError> {
    let mut out: Vec<Message> = Vec::new();
    let mut approvals: Vec<ApprovalDecision> = Vec::new();
    match v {
        Some(Value::String(s)) => out.push(Message::text(Role::User, s.clone())),
        Some(Value::Array(items)) => {
            for item in items {
                push_input_item(&mut out, &mut approvals, native_only, item)?;
            }
        }
        None | Some(Value::Null) => {}
        Some(other) => {
            return Err(GatewayError::BadRequest(format!(
                "'input' must be a string or an array, got {}",
                kind_of(other)
            )))
        }
    }
    Ok((out, approvals))
}

/// Whether `item` is an `item_reference`: by its type, or as the API also
/// takes one, an item with an `id` and neither a `type` nor a `role`.
pub(crate) fn is_item_reference(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some(t) => t == "item_reference",
        None => item.get("role").is_none() && item.get("id").and_then(Value::as_str).is_some(),
    }
}

fn push_input_item(
    out: &mut Vec<Message>,
    approvals: &mut Vec<ApprovalDecision>,
    native_only: &mut NativeOnlyParts,
    item: &Value,
) -> Result<(), GatewayError> {
    // An item with a `role` and no `type` is a message; `type: "message"` is
    // the explicit spelling of the same thing.
    let ty = item.get("type").and_then(Value::as_str);
    // A reference to an item the upstream stored: only it can read one.
    if is_item_reference(item) {
        native_only.note(
            "input item type 'item_reference'".into(),
            GatewayError::Unsupported("input item type 'item_reference'".into()),
        );
        return Ok(());
    }
    match ty {
        None | Some("message") => {
            let role = match item.get("role").and_then(Value::as_str) {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                Some("system") | Some("developer") => Role::System,
                other => {
                    return Err(GatewayError::BadRequest(format!(
                        "input message with role {other:?}"
                    )))
                }
            };
            let content = parse_item_content(item.get("content"), role, native_only)?;
            if content.is_empty() {
                return Ok(());
            }
            match out.last_mut() {
                // The turn's reasoning item came first and opened the
                // assistant message; this is the rest of that same turn.
                Some(m)
                    if role == Role::Assistant
                        && m.role == Role::Assistant
                        && m.content
                            .iter()
                            .all(|p| matches!(p, ContentPart::Reasoning { .. })) =>
                {
                    m.content.extend(content);
                }
                _ => out.push(Message { role, content }),
            }
        }
        Some("function_call") => {
            let call = ContentPart::ToolUse {
                id: str_at(item, "call_id")
                    .or_else(|| str_at(item, "id"))
                    .unwrap_or_default(),
                name: str_at(item, "name").unwrap_or_default(),
                args: parse_arguments(item.get("arguments")),
            };
            append_to_assistant(out, call);
        }
        // The gateway's own record of a tool it ran. Replayed back to it, this
        // must rebuild both halves — the call and its result — or the model
        // sees a result for a call it never made.
        Some("mcp_call") => {
            let id = str_at(item, "id").unwrap_or_default();
            let name = str_at(item, "name").unwrap_or_default();
            append_to_assistant(
                out,
                ContentPart::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    args: parse_arguments(item.get("arguments")),
                },
            );
            let error = item.get("error").and_then(Value::as_str);
            out.push(Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    id,
                    name: Some(name),
                    content: ToolResultBlock::one(
                        error
                            .map(String::from)
                            .or_else(|| str_at(item, "output"))
                            .unwrap_or_default(),
                    ),
                    is_error: error.is_some(),
                }],
            });
        }
        // `output` is a string or an array of items; the array is read into
        // blocks, so an `input_image` reaches the model as an image or its
        // placeholder rather than as base64 text (`tool_output`).
        Some("function_call_output") => {
            let id = str_at(item, "call_id").unwrap_or_default();
            let content = tool_output::output_blocks(item.get("output"), &id);
            out.push(Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    id,
                    name: None,
                    content,
                    is_error: false,
                }],
            })
        }
        // A verdict on a call an earlier response stopped at. Not a
        // conversation turn: it decides whether a *pending* call runs, and the
        // handler settles it against the stored response.
        Some("mcp_approval_response") => approvals.push(ApprovalDecision {
            approval_request_id: str_at(item, "approval_request_id")
                .or_else(|| str_at(item, "approval_request"))
                .ok_or_else(|| {
                    GatewayError::BadRequest(
                        "mcp_approval_response without 'approval_request_id'".into(),
                    )
                })?,
            approve: item
                .get("approve")
                .and_then(Value::as_bool)
                // No verdict is not consent. A malformed approval must fail
                // closed, or the field is decorative.
                .unwrap_or(false),
            reason: str_at(item, "reason"),
        }),
        // The gateway's own record that it *asked*. Replayed back with the
        // verdict alongside it, so it carries no new information — the pending
        // call itself is held server-side.
        Some("mcp_approval_request") => {}
        // The trace of an earlier turn, replayed. What this gateway writes
        // into the item's `summary` is the model's whole trace, not a digest,
        // so sending it back is faithful — and it is what llama-server's
        // `--reasoning-preserve` needs. A real Responses provider's items
        // never arrive here: those upstreams are passthrough. The item opens
        // the assistant turn; the `message` / `function_call` items that
        // follow it fold in.
        //
        // An `encrypted_content` this gateway's loop wrote (`lmgw-openai:…`,
        // openai egress design §7.3.2) is the item's seal again, kept with
        // an empty summary too: the seal is what the platform reads. One the
        // upstream wrote itself, from a native answer, is not read here yet:
        // the loop sends the item's summary as before.
        Some("reasoning") => {
            let text = reasoning_item_text(item);
            let sealed = match item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .and_then(crate::ir::Seal::from_carrier)
            {
                Some(read) => Some(read.map_err(GatewayError::BadRequest)?),
                None => None,
            };
            if !text.is_empty() || sealed.is_some() {
                out.push(Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::Reasoning {
                        text,
                        signature: None,
                        sealed,
                        signature_scope: None,
                    }],
                });
            }
        }
        // A hosted tool's record (`web_search_call`, `code_interpreter_call`,
        // …) or another item only the upstream reads: replayed to a native
        // upstream as sent, refused by the loop.
        Some(other) => native_only.note_using(
            format!("input item type '{other}'"),
            // A hosted tool's record (`*_call`, `*_call_output`) replays
            // a tool call.
            other.contains("_call").then_some(Facet::ToolCalls),
            GatewayError::Unsupported(format!("input item type '{other}'")),
        ),
    }
    Ok(())
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(String::from)
}

/// Text of a `reasoning` input item: the `summary` parts this gateway writes,
/// plus the raw `content` parts newer clients carry.
fn reasoning_item_text(item: &Value) -> String {
    ["summary", "content"]
        .iter()
        .filter_map(|k| item.get(*k).and_then(Value::as_array))
        .flatten()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `arguments` is a JSON *string* on the wire. An unparseable one becomes a
/// JSON string rather than an error — the tool's own validation says something
/// more useful than a 400 from here would.
fn parse_arguments(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => crate::ir::parse_call_args(s),
        Some(other) => other.clone(),
        None => json!({}),
    }
}

/// Tool calls belong to the assistant turn that made them; fold into the
/// trailing assistant message when there is one so a turn with several calls
/// doesn't become several one-call turns.
fn append_to_assistant(out: &mut Vec<Message>, part: ContentPart) {
    match out.last_mut() {
        Some(m) if m.role == Role::Assistant => m.content.push(part),
        _ => out.push(Message {
            role: Role::Assistant,
            content: vec![part],
        }),
    }
}

/// A message's content as IR parts. A part the loop has no reading for (an
/// `input_file`, an `input_image` by `file_id`, a type it does not know) is
/// noted as [`native_only`] and left out of the IR: the native path sends the
/// body as it came, and the loop refuses the request.
fn parse_item_content(
    v: Option<&Value>,
    role: Role,
    native_only: &mut NativeOnlyParts,
) -> Result<Vec<ContentPart>, GatewayError> {
    let mut parts = Vec::new();
    match v {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                parts.push(ContentPart::text(s.clone()));
            }
        }
        Some(Value::Array(items)) => {
            for p in items {
                match p.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") | Some("text") => {
                        parts.push(ContentPart::text(
                            p.get("text").and_then(Value::as_str).unwrap_or_default(),
                        ))
                    }
                    Some("input_image") => {
                        let url = p
                            .get("image_url")
                            .and_then(Value::as_str)
                            .or_else(|| p.pointer("/image_url/url").and_then(Value::as_str))
                            .unwrap_or_default();
                        if url.is_empty() {
                            native_only.note_using(
                                "input_image by file_id".into(),
                                Some(Facet::Vision),
                                GatewayError::Unsupported(
                                    "input_image by file_id — send an image URL or data: URI"
                                        .into(),
                                ),
                            );
                            continue;
                        }
                        parts.push(super::openai::parse_image_url(url));
                    }
                    Some("input_audio") => {
                        parts.push(super::openai::parse_input_audio(p.get("input_audio"))?);
                    }
                    // The model's refusal, sent back: an assistant part
                    // only, as OpenAI has it.
                    Some("refusal") if role != Role::Assistant => {
                        return Err(super::refusal_part_misplaced(role))
                    }
                    Some("refusal") => parts.push(ContentPart::Refusal {
                        text: p
                            .get("refusal")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    other => {
                        let e = GatewayError::Unsupported(format!("input content part {other:?}"));
                        match other {
                            Some(t) => native_only.note(format!("input content part '{t}'"), e),
                            None => return Err(e),
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(parts)
}

// ---------------------------------------------------------------------------
// Response / streaming
// ---------------------------------------------------------------------------

/// Which text item is currently open. Tool calls are tracked separately, in
/// [`ResponsesEncoder::calls`] — a turn can have several of them in flight at
/// once, so "the open item" is not a single slot for them.
#[derive(Debug, PartialEq)]
enum Open {
    None,
    Reasoning,
    Message,
    /// A message item whose one part is the model's `refusal`.
    Refusal,
}

/// OpenAI's `ResponseErrorCode` enum, as the pinned schema has it
/// (`tests/fixtures/api_schemas/openai.json`): what a failed Response's
/// `error.code` may say. The SDKs' types refuse any other word.
pub const RESPONSE_ERROR_CODES: &[&str] = &[
    "server_error",
    "rate_limit_exceeded",
    "invalid_prompt",
    "data_residency_mismatch",
    "bio_policy",
    "misalignment_policy_violation",
    "vector_store_timeout",
    "invalid_image",
    "invalid_image_format",
    "invalid_base64_image",
    "invalid_image_url",
    "image_too_large",
    "image_too_small",
    "image_parse_error",
    "image_content_policy_violation",
    "invalid_image_mode",
    "image_file_too_large",
    "unsupported_image_media_type",
    "empty_image_file",
    "failed_to_download_image",
    "image_file_not_found",
];

/// A failed run's `error.code`, in OpenAI's enum ([`RESPONSE_ERROR_CODES`];
/// api-compat design §9 finding 8): a failed upstream Response's own code
/// where it is one of them (openai egress design §7.2.3); a rate limit or an
/// exhausted quota `rate_limit_exceeded`; a refusal of the request — a
/// structured-output refusal (structured-output design §11.15), any error
/// answered with a 4xx — `invalid_prompt`; anything else `server_error`.
/// lmgw's own kind (`invalid_json_schema`, `upstream`, …) is in the request
/// log, and its text in `error.message`.
pub fn failed_code(e: &GatewayError) -> &'static str {
    if let Some(code) = e.failed_answer().and_then(|f| f.code.as_deref()) {
        if let Some(known) = RESPONSE_ERROR_CODES.iter().find(|c| **c == code) {
            return known;
        }
    }
    match e {
        GatewayError::UpstreamLimited { .. } | GatewayError::KeyRate { .. } => {
            "rate_limit_exceeded"
        }
        _ if e.about_format() || e.http_status().is_client_error() => "invalid_prompt",
        _ => "server_error",
    }
}

/// `incomplete_details.reason` of a run the output cap cut off.
pub const INCOMPLETE_MAX_OUTPUT_TOKENS: &str = "max_output_tokens";
/// `incomplete_details.reason` of a run a content filter stopped.
pub const INCOMPLETE_CONTENT_FILTER: &str = "content_filter";

/// A tool-call output item under construction.
struct CallItem {
    /// Its index in `output`, claimed by a placeholder as soon as the call
    /// starts so later items get honest `output_index`es.
    slot: usize,
    /// The item id (`mcp_…` / `fc_…` / `mcpr_…`).
    id: String,
    call_id: String,
    name: String,
    /// `Some(label)` for a gateway-run call, `None` for a client function tool.
    server_label: Option<String>,
    /// Gated behind an approval — rendered as `mcp_approval_request`, and no
    /// result will arrive in this response.
    needs_approval: bool,
    args: String,
    /// Its terminal item has been written; nothing more is coming.
    settled: bool,
    /// Its `output_item.added` went out. An approval request's does not
    /// until its arguments are known: OpenAI sends that item whole, in
    /// `added` and again in `done`, with no argument events.
    announced: bool,
}

/// Builds the Responses `output` array and — when streaming — the `response.*`
/// SSE sequence, from the loop's events.
///
/// **One state machine for both modes.** The non-streaming path runs the same
/// encoder and simply discards the frames, so a streamed and an unstreamed
/// response to the same request cannot drift apart; only the framing differs.
pub struct ResponsesEncoder {
    id: String,
    created: i64,
    alias: String,
    /// Monotonic across the whole response, as the API requires.
    seq: u64,
    /// Output items, in `output_index` order. A call's slot is filled with a
    /// placeholder when it opens and overwritten when it settles.
    output: Vec<Value>,
    /// The text item being built, if any, plus its id and accumulated text.
    open: Open,
    open_id: String,
    open_text: String,
    /// The open reasoning item's seal, which its `done` carries as
    /// `encrypted_content` (openai egress design §7.3.2).
    open_seal: Option<crate::ir::Seal>,
    /// Loop call ordinal → its item. Keyed per call rather than a single "open
    /// call", because a turn with three tool calls has three of them open at
    /// once: sharing one id and one argument buffer between them cross-wires
    /// the transcript.
    calls: std::collections::BTreeMap<usize, CallItem>,
    usage: Usage,
    status: &'static str,
    incomplete_reason: Option<String>,
    /// The failed run's `error.code` ([`failed_code`]: one of OpenAI's
    /// codes) and message (lmgw's whole text of the error).
    error: Option<(String, String)>,
    /// Request fields echoed on the response object.
    echo: Value,
    streaming: bool,
    /// The `service_tier` the last turn's `openai` answer reported
    /// ([`LoopEvent::ServiceTier`]): on the response object, as OpenAI's own
    /// says which tier served it. `None` on every other upstream.
    service_tier: Option<String>,
}

impl ResponsesEncoder {
    pub fn new(alias: &str, echo: Value, streaming: bool) -> Self {
        Self {
            id: format!("resp_{}", rand_id()),
            created: now_unix(),
            alias: alias.to_string(),
            seq: 0,
            output: Vec::new(),
            open: Open::None,
            open_id: String::new(),
            open_text: String::new(),
            open_seal: None,
            calls: std::collections::BTreeMap::new(),
            usage: Usage::default(),
            status: "in_progress",
            incomplete_reason: None,
            error: None,
            echo,
            streaming,
            service_tier: None,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// The response's terminal status, once [`on_event`](Self::on_event) has
    /// seen the run finish. What the store records.
    pub fn status(&self) -> &'static str {
        self.status
    }

    /// Token usage as reported by the run.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    fn ev(&mut self, ty: &str, mut body: Value) -> String {
        if !self.streaming {
            return String::new();
        }
        self.seq += 1;
        body["type"] = json!(ty);
        body["sequence_number"] = json!(self.seq);
        frame(Some(ty), &body.to_string())
    }

    /// `response.created` + `response.in_progress`.
    pub fn start(&mut self) -> String {
        let snap = self.snapshot();
        let mut out = self.ev(event::CREATED, json!({ "response": snap }));
        let snap = self.snapshot();
        out.push_str(&self.ev(event::IN_PROGRESS, json!({ "response": snap })));
        out
    }

    /// Record the tools a server exposed, as OpenAI's `mcp_list_tools` item.
    /// Emitted before the first turn so a client can see what the model was
    /// actually offered — the difference between "the model ignored the tool"
    /// and "the tool was never listed".
    pub fn push_mcp_list_tools(&mut self, label: &str, tools: &[ToolDef]) -> String {
        let item = json!({
            "id": format!("mcpl_{}", rand_id()),
            "type": "mcp_list_tools",
            "server_label": label,
            "tools": tools.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })).collect::<Vec<_>>(),
        });
        let index = self.output.len();
        self.output.push(item.clone());
        let mut out = self.ev(
            event::OUTPUT_ITEM_ADDED,
            json!({"output_index": index, "item": item}),
        );
        out.push_str(&self.ev(
            "response.mcp_list_tools.completed",
            json!({"output_index": index, "item_id": item["id"]}),
        ));
        out.push_str(&self.ev(
            event::OUTPUT_ITEM_DONE,
            json!({"output_index": index, "item": item}),
        ));
        out
    }

    /// An MCP server that could not be listed. Surfaced as a failed
    /// `mcp_list_tools` item rather than dropped, so a run that produced no
    /// tool calls because a server was down says so.
    pub fn push_mcp_list_failed(&mut self, label: &str, error: &str) -> String {
        let item = json!({
            "id": format!("mcpl_{}", rand_id()),
            "type": "mcp_list_tools",
            "server_label": label,
            "tools": [],
            "error": error,
        });
        let index = self.output.len();
        self.output.push(item.clone());
        let mut out = self.ev(
            event::OUTPUT_ITEM_ADDED,
            json!({"output_index": index, "item": item}),
        );
        out.push_str(&self.ev(
            "response.mcp_list_tools.failed",
            json!({"output_index": index, "item_id": item["id"], "error": error}),
        ));
        out.push_str(&self.ev(
            event::OUTPUT_ITEM_DONE,
            json!({"output_index": index, "item": item}),
        ));
        out
    }

    pub fn on_event(&mut self, ev: &LoopEvent) -> String {
        match ev {
            // A new turn starts a new message item; the previous one is final.
            LoopEvent::TurnStarted { .. } => self.close_open(),
            // Kept for the response object, as OpenAI's carries it (openai
            // egress design §5.7); no event of its own.
            LoopEvent::ServiceTier(t) => {
                self.service_tier = Some(t.clone());
                String::new()
            }
            // An item that opens here starts without the blank line the
            // platform's decoder puts between two items' summaries
            // (`egress/openai/responses/decode.rs`): that line joins a whole
            // trace, and this is one item's summary.
            LoopEvent::Reasoning(t) => {
                let t = match self.open == Open::Reasoning {
                    true => t.as_str(),
                    false => t.trim_start_matches('\n'),
                };
                if t.is_empty() {
                    return String::new();
                }
                let mut out = String::new();
                if self.open != Open::Reasoning {
                    out.push_str(&self.close_open());
                    self.open = Open::Reasoning;
                    self.open_id = format!("rs_{}", rand_id());
                    let index = self.output.len();
                    let id = self.open_id.clone();
                    out.push_str(&self.ev(
                        event::OUTPUT_ITEM_ADDED,
                        json!({"output_index": index, "item": {
                            "id": id, "type": item::REASONING, "summary": [],
                        }}),
                    ));
                    out.push_str(&self.ev(
                        "response.reasoning_summary_part.added",
                        json!({"output_index": index, "item_id": id, "summary_index": 0,
                               "part": {"type": part::SUMMARY_TEXT, "text": ""}}),
                    ));
                }
                self.open_text.push_str(t);
                let index = self.output.len();
                let id = self.open_id.clone();
                out.push_str(&self.ev(
                    event::REASONING_SUMMARY_TEXT_DELTA,
                    json!({"output_index": index, "item_id": id, "summary_index": 0,
                           "delta": t}),
                ));
                out
            }
            // The open reasoning item's seal closes it, its `done` carrying
            // the seal as `encrypted_content` in the carrier lmgw reads back
            // (openai egress design §7.3.2); an item that reasoned no
            // summary is an item of its own.
            LoopEvent::ReasoningSeal(seal) => {
                if self.open != Open::Reasoning {
                    let mut out = self.close_open();
                    out.push_str(&self.sealed_item(seal));
                    return out;
                }
                self.open_seal = Some(seal.clone());
                self.close_open()
            }
            LoopEvent::Text(t) => self.message_delta(t, Open::Message),
            // OpenAI's own part for it, streamed as `response.refusal.delta`.
            LoopEvent::Refusal(t) => self.message_delta(t, Open::Refusal),
            LoopEvent::CallStarted {
                index,
                call_id,
                name,
                server_label,
                needs_approval,
            } => {
                let (mut out, slot) =
                    self.open_call(*index, call_id, name, server_label, *needs_approval);
                let c = &self.calls[index];
                let (id, gated, server_side) =
                    (c.id.clone(), c.needs_approval, c.server_label.is_some());
                if server_side && !gated {
                    out.push_str(&self.ev(
                        "response.mcp_call.in_progress",
                        json!({"output_index": slot, "item_id": id}),
                    ));
                }
                out
            }
            LoopEvent::CallArgs { index, fragment } => {
                let Some(c) = self.calls.get_mut(index) else {
                    return String::new();
                };
                c.args.push_str(fragment);
                if c.needs_approval && c.server_label.is_some() {
                    return String::new();
                }
                let (slot, id, server_side) = (c.slot, c.id.clone(), c.server_label.is_some());
                let name = if server_side {
                    "response.mcp_call_arguments.delta"
                } else {
                    event::FUNCTION_CALL_ARGUMENTS_DELTA
                };
                self.ev(
                    name,
                    json!({"output_index": slot, "item_id": id, "delta": fragment}),
                )
            }
            LoopEvent::CallReady {
                index,
                call_id,
                name,
                args,
                server_label,
                needs_approval,
            } => {
                // A unary turn produces no CallStarted/CallArgs, so open the
                // item here if it isn't already; the streamed and unstreamed
                // paths converge on the same output item either way.
                let mut out = String::new();
                if !self.calls.contains_key(index) {
                    let (frames, _) =
                        self.open_call(*index, call_id, name, server_label, *needs_approval);
                    out.push_str(&frames);
                }
                let c = self.calls.get_mut(index).expect("just opened");
                // The arguments are what streamed in, exactly, as OpenAI's
                // `.done` is the deltas joined: a client that concatenates the
                // deltas onto the added item's `""` gets the same string as
                // the `.done` and the item. A call that streamed no text (a
                // unary turn's) streams its arguments here as one delta; a
                // call without arguments streamed `{}` already
                // (`egress::no_args`) or streams it now.
                let (slot, id, server_side, gated) = (
                    c.slot,
                    c.id.clone(),
                    c.server_label.is_some(),
                    c.needs_approval,
                );
                let approval = server_side && gated;
                if c.args.trim().is_empty() {
                    // Model text that did not parse is kept as a string
                    // (`ir::parse_call_args`): sent as it came, not quoted.
                    let whole = match args {
                        Value::String(text) => text.clone(),
                        other => serde_json::to_string(other)
                            .unwrap_or_else(|_| crate::egress::no_args::NO_ARGS.into()),
                    };
                    c.args.push_str(&whole);
                    if !approval {
                        let delta = if server_side {
                            "response.mcp_call_arguments.delta"
                        } else {
                            event::FUNCTION_CALL_ARGUMENTS_DELTA
                        };
                        out.push_str(&self.ev(
                            delta,
                            json!({"output_index": slot, "item_id": id, "delta": whole}),
                        ));
                    }
                }
                if approval {
                    // The whole item, arguments in, then done: no argument
                    // events, no `.completed` of its own (neither is in
                    // OpenAI's schema for it).
                    out.push_str(&self.settle(*index, "completed", None));
                    return out;
                }
                let arguments = self.calls[index].args.clone();
                let done = if server_side {
                    "response.mcp_call_arguments.done"
                } else {
                    "response.function_call_arguments.done"
                };
                out.push_str(&self.ev(
                    done,
                    json!({"output_index": slot, "item_id": id, "arguments": arguments}),
                ));
                // Both of these are final the moment the arguments are known: a
                // client tool because nothing here will run it, an approval
                // request because the answer comes in the *next* request.
                if !server_side {
                    out.push_str(&self.settle(*index, "completed", None));
                }
                out
            }
            LoopEvent::CallResult {
                index,
                blocks,
                is_error,
                ..
            } => {
                let Some(c) = self.calls.get(index) else {
                    return String::new();
                };
                let (slot, id) = (c.slot, c.id.clone());
                // `mcp_call.output` is a string in the Responses schema. That
                // is only the client's transcript — the *model* received the
                // blocks themselves via the IR, so nothing was lost upstream.
                let (text, _) = flatten_tool_result(blocks);
                let ev_name = if *is_error {
                    "response.mcp_call.failed"
                } else {
                    "response.mcp_call.completed"
                };
                let mut out = self.ev(ev_name, json!({"output_index": slot, "item_id": id}));
                out.push_str(&self.settle(*index, "completed", Some((text, *is_error))));
                out
            }
            LoopEvent::Done { reason, usage } => {
                self.usage = *usage;
                // The status first: a message cut off by the cap or the
                // filter is `incomplete` inside an incomplete response.
                match reason {
                    StopReason::Incomplete(r) => {
                        self.status = "incomplete";
                        self.incomplete_reason = Some((*r).to_string());
                    }
                    // The model was cut off, as OpenAI's Responses reports
                    // it: `incomplete`, with the reason, and the stream's
                    // `response.incomplete` (`finish`) for its
                    // `response.completed`. Chat and messages say the same
                    // with `length` / `max_tokens`, and a content filter's
                    // `content_filter`.
                    StopReason::Done(FinishReason::Length) => {
                        self.status = "incomplete";
                        self.incomplete_reason = Some(INCOMPLETE_MAX_OUTPUT_TOKENS.into());
                    }
                    StopReason::Done(FinishReason::ContentFilter) => {
                        self.status = "incomplete";
                        self.incomplete_reason = Some(INCOMPLETE_CONTENT_FILTER.into());
                    }
                    // An approval-gated run reports `completed`, as OpenAI's
                    // does: the response *is* finished, and the approval
                    // requests in `output` are what the client acts on. Calling
                    // it incomplete would make SDK helpers that gate on
                    // `status == "completed"` treat a normal round trip as a
                    // failure.
                    _ => self.status = "completed",
                }
                let mut out = self.close_open();
                out.push_str(&self.close_unsettled_calls());
                out.push_str(&self.finish());
                out
            }
        }
    }

    /// Claim an `output` slot for a tool call and announce the item.
    ///
    /// The placeholder goes into `output` immediately: without it `output.len()`
    /// would not advance, and the *next* item — another call, or the message
    /// after it — would be announced at an `output_index` already in use.
    fn open_call(
        &mut self,
        index: usize,
        call_id: &str,
        name: &str,
        server_label: &Option<String>,
        needs_approval: bool,
    ) -> (String, usize) {
        let mut out = self.close_open();
        let slot = self.output.len();
        let id = match (server_label, needs_approval) {
            // An approval item's id is the one the client quotes back in its
            // `mcp_approval_response`, so it must be the id the loop keyed the
            // pending call on — not a fresh random one it could never match.
            (Some(_), true) => crate::agent::approval_id(call_id, index),
            (Some(_), false) => format!("mcp_{}", rand_id()),
            (None, _) => format!("fc_{}", rand_id()),
        };
        let call = CallItem {
            slot,
            id,
            call_id: call_id.to_string(),
            name: name.to_string(),
            server_label: server_label.clone(),
            needs_approval,
            args: String::new(),
            settled: false,
            announced: !(server_label.is_some() && needs_approval),
        };
        let item = Self::call_item(&call, "in_progress", None);
        self.output.push(item.clone());
        let announced = call.announced;
        self.calls.insert(index, call);
        if announced {
            out.push_str(&self.ev(
                event::OUTPUT_ITEM_ADDED,
                json!({"output_index": slot, "item": item}),
            ));
        }
        (out, slot)
    }

    /// Write a call's final item into its slot and emit its `output_item.done`.
    fn settle(&mut self, index: usize, status: &str, result: Option<(String, bool)>) -> String {
        let Some(call) = self.calls.get_mut(&index) else {
            return String::new();
        };
        call.settled = true;
        let slot = call.slot;
        let item = Self::call_item(call, status, result);
        let mut out = String::new();
        if !call.announced {
            call.announced = true;
            out.push_str(&self.ev(
                event::OUTPUT_ITEM_ADDED,
                json!({"output_index": slot, "item": item}),
            ));
        }
        self.output[slot] = item.clone();
        out.push_str(&self.ev(
            event::OUTPUT_ITEM_DONE,
            json!({"output_index": slot, "item": item}),
        ));
        out
    }

    /// Close every call that never reached a terminal state — a run cut short
    /// by a budget, or the ungated siblings of a gated call, which wait for the
    /// next request. Marked `incomplete` rather than left dangling at
    /// `in_progress`, so a client can tell "still running" from "never ran".
    fn close_unsettled_calls(&mut self) -> String {
        let open: Vec<usize> = self
            .calls
            .iter()
            .filter(|(_, c)| !c.settled)
            .map(|(i, _)| *i)
            .collect();
        let mut out = String::new();
        for i in open {
            out.push_str(&self.settle(i, "incomplete", None));
        }
        out
    }

    /// Terminal frames. Separate from [`on_event`] so a failure before the loop
    /// even starts can still close the response properly.
    pub fn finish(&mut self) -> String {
        let snap = self.snapshot();
        let ty = match self.status {
            "incomplete" => event::INCOMPLETE,
            "failed" => event::FAILED,
            _ => event::COMPLETED,
        };
        self.ev(ty, json!({ "response": snap }))
    }

    /// Abandon the run with an error — used when a turn fails outright, where
    /// the client still needs a terminal frame naming the reason.
    pub fn fail(&mut self, e: &GatewayError) -> String {
        self.status = "failed";
        self.error = Some((failed_code(e).to_string(), e.to_string()));
        let mut out = self.close_open();
        out.push_str(&self.finish());
        out
    }

    /// One call's `output` item, in whichever of the three shapes applies.
    ///
    /// An associated function, not a method: it reads only the call's own
    /// state, which is exactly the property that keeps two concurrent calls
    /// from sharing an id or an argument buffer.
    fn call_item(call: &CallItem, status: &str, result: Option<(String, bool)>) -> Value {
        // In progress: what has streamed so far, `""` at the start, as
        // OpenAI's added item has it. Settled without any text (a run cut
        // before the call's arguments came): a call with none, `{}`.
        let args = match (status, call.args.trim().is_empty()) {
            ("in_progress", _) | (_, false) => call.args.clone(),
            _ => crate::egress::no_args::NO_ARGS.to_string(),
        };
        match (&call.server_label, call.needs_approval) {
            // Gated: the client is being asked, not told. It carries no
            // output/error — the result, if any, belongs to the next response.
            (Some(label), true) => json!({
                "id": call.id,
                "type": "mcp_approval_request",
                "server_label": label,
                "name": call.name,
                "arguments": args,
            }),
            (Some(label), false) => {
                let mut item = json!({
                    "id": call.id,
                    "type": "mcp_call",
                    "status": status,
                    "server_label": label,
                    "name": call.name,
                    "arguments": args,
                });
                match result {
                    Some((text, true)) => {
                        item["error"] = json!(text);
                        item["output"] = Value::Null;
                    }
                    Some((text, false)) => {
                        item["output"] = json!(text);
                        item["error"] = Value::Null;
                    }
                    None => {}
                }
                item
            }
            (None, _) => json!({
                "id": call.id,
                "type": item::FUNCTION_CALL,
                "status": status,
                "call_id": call.call_id,
                "name": call.name,
                "arguments": args,
            }),
        }
    }

    /// A reasoning item that came with its seal and no summary: added and
    /// done at once, its `encrypted_content` the seal's carrier.
    fn sealed_item(&mut self, seal: &crate::ir::Seal) -> String {
        let index = self.output.len();
        let id = format!("rs_{}", rand_id());
        let mut out = self.ev(
            event::OUTPUT_ITEM_ADDED,
            json!({"output_index": index, "item": {
                "id": id, "type": item::REASONING, "summary": [],
            }}),
        );
        let item = json!({
            "id": id, "type": item::REASONING, "summary": [],
            "encrypted_content": seal.carrier(),
        });
        self.output.push(item.clone());
        out.push_str(&self.ev(
            event::OUTPUT_ITEM_DONE,
            json!({"output_index": index, "item": item}),
        ));
        out
    }

    /// `t` added to the open message item of `kind` (`Message` for text,
    /// `Refusal` for a refusal), which is opened first when another is.
    fn message_delta(&mut self, t: &str, kind: Open) -> String {
        let mut out = String::new();
        let refusal = kind == Open::Refusal;
        if self.open != kind {
            out.push_str(&self.close_open());
            self.open = kind;
            self.open_id = format!("msg_{}", rand_id());
            let index = self.output.len();
            let id = self.open_id.clone();
            out.push_str(&self.ev(
                event::OUTPUT_ITEM_ADDED,
                json!({"output_index": index, "item": {
                    "id": id, "type": item::MESSAGE, "status": "in_progress",
                    "role": "assistant", "content": [],
                }}),
            ));
            let empty = match refusal {
                true => json!({"type": part::REFUSAL, "refusal": ""}),
                false => json!({"type": part::OUTPUT_TEXT, "text": "", "annotations": [],
                               "logprobs": []}),
            };
            out.push_str(&self.ev(
                "response.content_part.added",
                json!({"output_index": index, "item_id": id, "content_index": 0,
                       "part": empty}),
            ));
        }
        self.open_text.push_str(t);
        let index = self.output.len();
        let id = self.open_id.clone();
        let name = match refusal {
            true => event::REFUSAL_DELTA,
            false => event::OUTPUT_TEXT_DELTA,
        };
        let mut delta =
            json!({"output_index": index, "item_id": id, "content_index": 0, "delta": t});
        // A text delta's `logprobs` is required by OpenAI's schema: empty,
        // as lmgw relays no log probabilities yet. A refusal delta has none.
        if !refusal {
            delta["logprobs"] = json!([]);
        }
        out.push_str(&self.ev(name, delta));
        out
    }

    fn close_open(&mut self) -> String {
        let index = self.output.len();
        let id = self.open_id.clone();
        let text = std::mem::take(&mut self.open_text);
        match std::mem::replace(&mut self.open, Open::None) {
            Open::None => String::new(),
            Open::Reasoning => {
                let mut item = json!({
                    "id": id, "type": item::REASONING,
                    "summary": [{"type": part::SUMMARY_TEXT, "text": text}],
                });
                if let Some(seal) = self.open_seal.take() {
                    item["encrypted_content"] = json!(seal.carrier());
                }
                let mut out = self.ev(
                    "response.reasoning_summary_text.done",
                    json!({"output_index": index, "item_id": id, "summary_index": 0,
                           "text": text}),
                );
                out.push_str(&self.ev(
                    "response.reasoning_summary_part.done",
                    json!({"output_index": index, "item_id": id, "summary_index": 0,
                           "part": {"type": part::SUMMARY_TEXT, "text": text}}),
                ));
                self.output.push(item.clone());
                out.push_str(&self.ev(
                    event::OUTPUT_ITEM_DONE,
                    json!({"output_index": index, "item": item}),
                ));
                out
            }
            kind @ (Open::Message | Open::Refusal) => {
                let refusal = kind == Open::Refusal;
                let part = match refusal {
                    true => json!({"type": part::REFUSAL, "refusal": text}),
                    false => json!({"type": part::OUTPUT_TEXT, "text": text, "annotations": [],
                                   "logprobs": []}),
                };
                // A run that failed mid-answer leaves its text unfinished;
                // the item says so rather than claiming it completed.
                let status = if matches!(self.status, "failed" | "incomplete") {
                    "incomplete"
                } else {
                    "completed"
                };
                let item = json!({
                    "id": id, "type": item::MESSAGE, "status": status,
                    "role": "assistant", "content": [part],
                });
                let mut out = match refusal {
                    true => self.ev(
                        "response.refusal.done",
                        json!({"output_index": index, "item_id": id, "content_index": 0,
                               "refusal": text}),
                    ),
                    false => self.ev(
                        "response.output_text.done",
                        json!({"output_index": index, "item_id": id, "content_index": 0,
                               "text": text, "logprobs": []}),
                    ),
                };
                out.push_str(&self.ev(
                    "response.content_part.done",
                    json!({"output_index": index, "item_id": id, "content_index": 0,
                           "part": part}),
                ));
                self.output.push(item.clone());
                out.push_str(&self.ev(
                    event::OUTPUT_ITEM_DONE,
                    json!({"output_index": index, "item": item}),
                ));
                out
            }
        }
    }

    /// The `response` object as it stands. This *is* the non-streaming body.
    pub fn snapshot(&self) -> Value {
        let p = self.usage.prompt_tokens.unwrap_or(0);
        let c = self.usage.completion_tokens.unwrap_or(0);
        let mut v = json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created,
            "status": self.status,
            "model": self.alias,
            "output": self.output,
            "error": self.error.as_ref().map(|(code, m)| json!({
                "code": code, "message": m,
            })).unwrap_or(Value::Null),
            "incomplete_details": self.incomplete_reason.as_ref()
                .map(|r| json!({"reason": r}))
                .unwrap_or(Value::Null),
            // The detail counters were hardcoded to 0 — a zero that reads as
            // "nothing was cached" when it meant "we never looked". The IR
            // carries the real numbers now; a provider that reports nothing
            // still yields 0 here, because Responses' shape has no way to say
            // "unknown" and inventing one would be worse than the schema.
            // `cache_write_tokens` is required beside `cached_tokens` in
            // OpenAI's current shape, so it is always there too.
            "usage": {
                "input_tokens": p,
                "input_tokens_details": {
                    "cached_tokens": self.usage.cached_input_tokens.unwrap_or(0),
                    "cache_write_tokens": self.usage.cache_write_tokens.unwrap_or(0),
                },
                "output_tokens": c,
                "output_tokens_details": {
                    "reasoning_tokens": self.usage.reasoning_tokens.unwrap_or(0),
                },
                "total_tokens": p + c,
            },
            // Overridden from `echo` with what the gateway actually did, so a
            // client can see whether `previous_response_id` will work on this
            // response before it tries.
            "store": false,
            // Required by OpenAI's schema (nullable): no access program
            // applies to an answer lmgw's loop made.
            "access_programs": Value::Null,
        });
        if let Some(obj) = self.echo.as_object() {
            for (k, val) in obj {
                v[k.as_str()] = val.clone();
            }
        }
        // The tier that served it, over any the request's echo carried: the
        // response says what happened, which may not be what was asked
        // (openai egress design §5.7).
        if let Some(tier) = &self.service_tier {
            v["service_tier"] = json!(tier);
        }
        v
    }
}

/// Convenience for the `output_text` shortcut most SDKs expose.
pub fn output_text(response: &Value) -> String {
    response
        .get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|i| i.get("type").and_then(Value::as_str) == Some("message"))
                .filter_map(|i| i.get("content").and_then(Value::as_array))
                .flatten()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Error body in the Responses shape (same envelope as chat-completions;
/// a structured-output error's `param` is `text.format`, structured-output
/// design §8).
pub fn serialize_error(e: &GatewayError) -> Value {
    e.to_responses_json()
}
