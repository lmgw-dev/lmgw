//! The one list of violations the check knows of and lmgw has not fixed yet.
//!
//! Every entry is a body lmgw sends (or an answer it gives) that the
//! provider's schema refuses, kept here because the fix is larger than the
//! check or needs a decision first. A matching violation does not fail the
//! test, but it is printed every time it matches ("api-compat known
//! violation, not fixed yet"), so it never goes quiet; a fixed one is taken
//! off the list in the fixing change. A schema or test-fixture quirk (the
//! provider's spec says less than its server takes, a fake upstream answers
//! something no provider would) is listed the same way, its reason saying
//! so.

use serde_json::Value;

use super::diagnose::Leaf;
use super::schemas::Entry;

pub struct Known {
    /// The entry point, as [`Entry`] displays it ("Anthropic messages_request").
    pub entry: &'static str,
    /// A JSON-pointer pattern: `*` is one segment, `**` any number.
    pub path: &'static str,
    /// The schema keyword that fails.
    pub keyword: &'static str,
    /// For an `additionalProperties` failure, the unknown keys this entry
    /// covers (every unknown key must be one of them); empty for any.
    pub keys: &'static [&'static str],
    /// For a `required` failure, the missing properties this entry covers
    /// (the missing one must be one of them); empty for any.
    pub missing: &'static [&'static str],
    /// Which failing values the entry covers ([`ANY`] for all).
    pub when: fn(&Value) -> bool,
    /// What is wrong and why it is not fixed yet.
    pub reason: &'static str,
}

const ANY: fn(&Value) -> bool = |_| true;

/// Text that is not blank and holds no JSON: a model's broken arguments. A
/// blank string or `null` is the bug of d3fa11d1 and stays a violation.
fn unparseable_text(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| !s.trim().is_empty() && serde_json::from_str::<Value>(s).is_err())
}

/// A `text.format` in llama.cpp's spelling that reached the platform: a
/// `json_object` carrying its `schema`, or a `json_schema` without one.
fn llama_spelling(v: &Value) -> bool {
    match v.get("type").and_then(Value::as_str) {
        Some("json_object") => v.get("schema").is_some(),
        Some("json_schema") => v.get("schema").is_none(),
        _ => false,
    }
}

/// The tiers the budget_floors suites' fake upstream reports.
fn fixture_tier(v: &Value) -> bool {
    matches!(v.as_str(), Some("turbo" | "ultrafast"))
}

/// A response_format of a type lmgw does not model, which it forwards as the
/// client wrote it: `regex` (llama.cpp's) is what the suites send.
fn unmodelled_format(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str) == Some("regex")
}

/// The extra keys inside a response_format that the suites' clients send
/// (`x-vendor` inside the json_schema object, `x-vendor-hint` beside it).
const VENDOR_KEYS: &[&str] = &["x-vendor", "x-vendor-hint"];

const SAMPLING: &[&str] = &["temperature", "top_p", "top_k"];

const SAMPLING_REASON: &str =
    "Anthropic: the official SDK's request type (2.4.0) has no temperature, top_p or top_k any \
     more, while older models still take them. The owner's rule (2026-11-04): the egress sends \
     the client's until the model refuses one, and a refusal leaves exactly that one out of \
     that model's sends from then on (model-capabilities design §5.7); never pre-emptively. \
     So a body that carries them is expected here.";

const UNPARSEABLE_ARGS: &str =
    "A stored tool call whose arguments are text that is not JSON (a model's broken output) is \
     replayed as that text, where the wire wants an object; ir::object_call_args sends it as it \
     is on purpose so nothing is invented (d3fa11d1). How to send it instead needs a decision. \
     Not fixed yet.";

const NO_STRICT: &str =
    "OpenAI's spec lists a function tool's `strict` as required (nullable); lmgw leaves it out \
     for a tool whose caller said nothing (ToolDef::strict None: the upstream's default), on the \
     request and in the Response's echo of its tools. Whether `null` means the same default \
     needs checking against the live API. Not fixed yet.";

const FORMAT_EXTRAS: &str =
    "The client's own keys inside response_format, forwarded verbatim by design \
     (structured-output design, `extra`/`format_extra`); OpenAI refuses unknown keys. The check \
     treats only a client's top-level keys as the client's yet. Not fixed yet.";

const FORMAT_ON_PLATFORM: &str =
    "llama.cpp's structured-output spellings (json_object with a schema, json_schema without \
     its json_schema object) and the client's extras reach the OpenAI platform's text.format, \
     which refuses them; translating or refusing them there needs a decision. Not fixed yet.";

const TOOL_CALL_BUDGET: &str =
    "A Responses loop that hits its tool-call ceiling ends incomplete with the reason \
     `max_tool_calls`; OpenAI's schema enumerates max_output_tokens and content_filter only. \
     Which of its reasons that run should report needs a decision. Not fixed yet.";

const MCP_CALL_ERROR: &str =
    "An mcp_call item's error is lmgw's text; OpenAI's current schema makes it an object \
     (mcp_protocol_error, mcp_tool_execution_error or http_error). Not fixed yet.";

/// Anthropic's beta compaction block, whole or as its stream delta.
fn compaction_block(v: &Value) -> bool {
    matches!(
        v.get("type").and_then(Value::as_str),
        Some("compaction" | "compaction_delta")
    )
}

/// The beta stop reason of a compaction request or a paused compaction.
fn compaction_stop(v: &Value) -> bool {
    v.as_str() == Some("compaction")
}

const COMPACTION: &str =
    "A schema quirk: Anthropic's compaction block and its compaction_delta (betas \
     compact-2026-09-04 and compact-2026-01-12) are not in the pinned schema, the SDK's non-beta \
     types; lmgw relays them as Anthropic wrote them (gateway design §6.1), and the SDK's beta \
     types (BetaCompactionBlock, BetaCompactionContentBlockDelta) take them, as their \
     BetaStopReason takes the stop reason `compaction`.";

/// The block type `anthropic_verbatim_blocks` invents, whole or as its
/// stream delta.
fn invented_block(v: &Value) -> bool {
    matches!(
        v.get("type").and_then(Value::as_str),
        Some("future_block" | "future_delta")
    )
}

const INVENTED_BLOCK: &str =
    "A test-fixture quirk: anthropic_verbatim_blocks invents the block type future_block (and \
     its future_delta), which no schema has, to stand for a type Anthropic adds after the pin; \
     lmgw carries any block type it does not model verbatim, both ways (gateway design §6.1, \
     changed 2026-11-05).";

pub const KNOWN: &[Known] = &[
    Known {
        entry: "Anthropic message",
        path: "/content/*",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: invented_block,
        reason: INVENTED_BLOCK,
    },
    Known {
        entry: "Anthropic stream_event",
        path: "/content_block",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: invented_block,
        reason: INVENTED_BLOCK,
    },
    Known {
        entry: "Anthropic stream_event",
        path: "/delta",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: invented_block,
        reason: INVENTED_BLOCK,
    },
    Known {
        entry: "Anthropic messages_request",
        path: "/messages/*/content/*",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: invented_block,
        reason: INVENTED_BLOCK,
    },
    Known {
        entry: "Anthropic message",
        path: "/content/*",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: compaction_block,
        reason: COMPACTION,
    },
    Known {
        entry: "Anthropic stream_event",
        path: "/content_block",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: compaction_block,
        reason: COMPACTION,
    },
    Known {
        entry: "Anthropic stream_event",
        path: "/delta",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: compaction_block,
        reason: COMPACTION,
    },
    Known {
        entry: "Anthropic message",
        path: "/stop_reason",
        keyword: "enum",
        keys: &[],
        missing: &[],
        when: compaction_stop,
        reason: COMPACTION,
    },
    Known {
        entry: "Anthropic stream_event",
        path: "/delta/stop_reason",
        keyword: "enum",
        keys: &[],
        missing: &[],
        when: compaction_stop,
        reason: COMPACTION,
    },
    Known {
        entry: "Anthropic messages_request",
        path: "",
        keyword: "additionalProperties",
        keys: SAMPLING,
        missing: &[],
        when: ANY,
        reason: SAMPLING_REASON,
    },
    Known {
        entry: "Anthropic messages_request_streaming",
        path: "",
        keyword: "additionalProperties",
        keys: SAMPLING,
        missing: &[],
        when: ANY,
        reason: SAMPLING_REASON,
    },
    Known {
        entry: "Anthropic messages_request",
        path: "/messages/*/content/*/input",
        keyword: "type",
        keys: &[],
        missing: &[],
        when: unparseable_text,
        reason: UNPARSEABLE_ARGS,
    },
    Known {
        entry: "Anthropic messages_request_streaming",
        path: "/messages/*/content/*/input",
        keyword: "type",
        keys: &[],
        missing: &[],
        when: unparseable_text,
        reason: UNPARSEABLE_ARGS,
    },
    Known {
        entry: "Gemini generate_content_request",
        path: "/contents/*/parts/*/functionCall/args",
        keyword: "type",
        keys: &[],
        missing: &[],
        when: unparseable_text,
        reason: UNPARSEABLE_ARGS,
    },
    Known {
        entry: "OpenAI responses_request",
        path: "/tools/*",
        keyword: "required",
        keys: &[],
        missing: &["strict"],
        when: ANY,
        reason: NO_STRICT,
    },
    Known {
        entry: "OpenAI response",
        path: "/tools/*",
        keyword: "required",
        keys: &[],
        missing: &["strict"],
        when: ANY,
        reason: NO_STRICT,
    },
    Known {
        entry: "OpenAI responses_stream_event",
        path: "/response/tools/*",
        keyword: "required",
        keys: &[],
        missing: &["strict"],
        when: ANY,
        reason: NO_STRICT,
    },
    Known {
        entry: "OpenAI chat_request",
        path: "/response_format/**",
        keyword: "additionalProperties",
        keys: VENDOR_KEYS,
        missing: &[],
        when: ANY,
        reason: FORMAT_EXTRAS,
    },
    Known {
        entry: "OpenAI chat_request",
        path: "/response_format",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: unmodelled_format,
        reason: "A response_format type lmgw does not model (llama.cpp's `regex`, \
                 ResponseFormat::Other) goes to an OpenAI-shaped row as the client wrote it, for \
                 the upstream to judge (structured-output design §9). Not fixed yet.",
    },
    Known {
        entry: "OpenAI responses_request",
        path: "/text/format",
        keyword: "additionalProperties",
        keys: &["schema", "x-vendor", "x-vendor-hint"],
        missing: &[],
        when: llama_spelling,
        reason: FORMAT_ON_PLATFORM,
    },
    Known {
        entry: "OpenAI responses_request",
        path: "/text/format",
        keyword: "additionalProperties",
        keys: VENDOR_KEYS,
        missing: &[],
        when: ANY,
        reason: FORMAT_EXTRAS,
    },
    Known {
        entry: "OpenAI responses_request",
        path: "/text/format",
        keyword: "required",
        keys: &[],
        missing: &["name", "schema"],
        when: llama_spelling,
        reason: FORMAT_ON_PLATFORM,
    },
    Known {
        entry: "OpenAI chat_request",
        path: "/messages",
        keyword: "minItems",
        keys: &[],
        missing: &[],
        when: ANY,
        reason: "An empty conversation is sent on as it is: a test client's own empty messages \
                 list (key_policy), and a realtime response.create with nothing in the \
                 conversation yet (realtime_turn). Refusing it before the upstream does needs a \
                 decision. Not fixed yet.",
    },
    Known {
        entry: "OpenAI response",
        path: "/output/*/error",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: ANY,
        reason: MCP_CALL_ERROR,
    },
    Known {
        entry: "OpenAI responses_stream_event",
        path: "/item/error",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: ANY,
        reason: MCP_CALL_ERROR,
    },
    Known {
        entry: "OpenAI responses_stream_event",
        path: "/response/output/*/error",
        keyword: "anyOf",
        keys: &[],
        missing: &[],
        when: ANY,
        reason: MCP_CALL_ERROR,
    },
    Known {
        entry: "OpenAI response",
        path: "/incomplete_details/reason",
        keyword: "enum",
        keys: &[],
        missing: &[],
        when: ANY,
        reason: TOOL_CALL_BUDGET,
    },
    Known {
        entry: "OpenAI responses_stream_event",
        path: "/response/incomplete_details/reason",
        keyword: "enum",
        keys: &[],
        missing: &[],
        when: ANY,
        reason: TOOL_CALL_BUDGET,
    },
    Known {
        entry: "OpenAI chat_response",
        path: "/service_tier",
        keyword: "enum",
        keys: &[],
        missing: &[],
        when: fixture_tier,
        reason: "Test-fixture quirk: lmgw answers the tier the upstream reported, verbatim \
                 (openai egress design §5.7), and the budget_floors suites' fake upstream \
                 reports tiers no provider has (turbo, ultrafast). Nothing to fix in lmgw.",
    },
];

/// The entry `leaf` is, if any.
pub fn matching(entry: Entry, leaf: &Leaf) -> Option<&'static Known> {
    let name = entry.to_string();
    KNOWN.iter().find(|k| {
        k.entry == name
            && k.keyword == leaf.keyword
            && glob(k.path, &leaf.path)
            && (k.keys.is_empty() || leaf.unexpected.iter().all(|u| k.keys.contains(&u.as_str())))
            && (k.missing.is_empty()
                || leaf
                    .missing
                    .as_deref()
                    .is_some_and(|m| k.missing.contains(&m)))
            && (k.when)(&leaf.value)
    })
}

/// `path` matches `pattern` (both JSON pointers; `*` one segment, `**` any).
pub fn glob(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').skip(1).collect();
    let s: Vec<&str> = path.split('/').skip(1).collect();
    fn go(p: &[&str], s: &[&str]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(&"**"), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some(&"*"), Some(_)) => go(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    go(&p, &s)
}
