//! Items → the chat model's request, without a socket (realtime design
//! §7.2, §7.4, §7.6, §16 "item ordering and rendering normalization").
//!
//! The conversations here are the ones strict templates refuse as they
//! stand: two user turns in a row, an assistant turn with nothing heard, the
//! user speaking while a client tool runs, a tool whose result has not
//! arrived, system items mid-conversation.

use lmgw_core::ir::{ContentPart, Message, Role, ToolChoice};
use lmgw_core::realtime::protocol::{Item, MaxOutputTokens, Tool};
use lmgw_core::realtime::render::{
    reasoning_control, render, strip_schema_keys, Input, CONTINUE, CONVERSATION_START,
    NO_RESULT_YET,
};
use serde_json::{json, Value};

fn item(v: Value) -> Item {
    serde_json::from_value(v).unwrap()
}

fn user(text: &str) -> Item {
    item(json!({"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": text}]}))
}

fn heard(transcript: &str) -> Item {
    item(json!({"type": "message", "role": "user",
                "content": [{"type": "input_audio", "transcript": transcript}]}))
}

fn assistant(text: &str) -> Item {
    item(json!({"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": text}]}))
}

fn call(call_id: &str, name: &str, args: &str) -> Item {
    item(
        json!({"type": "function_call", "call_id": call_id, "name": name,
                "arguments": args, "status": "completed"}),
    )
}

fn output(call_id: &str, out: &str) -> Item {
    item(json!({"type": "function_call_output", "call_id": call_id, "output": out}))
}

fn messages(items: &[Item], instructions: &str) -> Vec<Message> {
    render(&Input {
        alias: "chatty",
        instructions,
        items,
        tools: &[],
        tool_choice: None,
        parallel_tool_calls: None,
        max_output_tokens: None,
        reasoning: None,
        speech_hint: None,
        written: &|_| None,
        mcp: Default::default(),
    })
    .messages
}

/// Each message as `(role, text)`, a tool call as `call:<id>:<name>` and a
/// tool result as `result:<id>:<name>:<text>`.
fn shape(ms: &[Message]) -> Vec<(Role, Vec<String>)> {
    ms.iter()
        .map(|m| {
            let parts = m
                .content
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => text.clone(),
                    ContentPart::ToolUse { id, name, .. } => format!("call:{id}:{name}"),
                    ContentPart::ToolResult {
                        id, name, content, ..
                    } => format!(
                        "result:{id}:{}:{}",
                        name.as_deref().unwrap_or("?"),
                        lmgw_core::ir::flatten_tool_result(content).0
                    ),
                    other => panic!("unexpected part {other:?}"),
                })
                .collect();
            (m.role, parts)
        })
        .collect()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn instructions_lead_and_consecutive_user_items_merge() {
    let items = [
        user("Hallo"),
        heard("wie spät"),
        heard("ist es?"),
        assistant("Zwölf."),
    ];
    let got = shape(&messages(&items, "Be brief."));
    assert_eq!(
        got,
        vec![
            (Role::System, s(&["Be brief."])),
            (Role::User, s(&["Hallo\nwie spät\nist es?"])),
            (Role::Assistant, s(&["Zwölf."])),
            // Never a trailing assistant turn: see below.
            (Role::User, s(&[CONTINUE])),
        ]
    );
}

#[test]
fn the_model_always_speaks_after_a_user_turn() {
    // The agent greeted first, then the user answered: strict templates want
    // the user first.
    let items = [
        assistant("Hallo, wie kann ich helfen?"),
        heard("Wie spät ist es?"),
    ];
    assert_eq!(
        shape(&messages(&items, "Be brief.")),
        vec![
            (Role::System, s(&["Be brief."])),
            (Role::User, s(&[CONVERSATION_START])),
            (Role::Assistant, s(&["Hallo, wie kann ich helfen?"])),
            (Role::User, s(&["Wie spät ist es?"])),
        ]
    );

    // A second response.create with nothing said in between: a trailing
    // assistant turn would be a prefill to extend (llama-server refuses one
    // with reasoning on), so the model is asked to go on instead.
    let items = [user("Erzähl was."), assistant("Es war einmal")];
    assert_eq!(
        shape(&messages(&items, "")),
        vec![
            (Role::User, s(&["Erzähl was."])),
            (Role::Assistant, s(&["Es war einmal"])),
            (Role::User, s(&[CONTINUE])),
        ]
    );

    // Both at once: the assistant's only turn.
    assert_eq!(
        shape(&messages(&[assistant("Hallo!")], "")),
        vec![
            (Role::User, s(&[CONVERSATION_START])),
            (Role::Assistant, s(&["Hallo!"])),
            (Role::User, s(&[CONTINUE])),
        ]
    );
    // An empty conversation stays empty: nothing to normalise.
    assert!(messages(&[], "").is_empty());
}

#[test]
fn empty_items_are_dropped_and_system_items_join_the_instructions() {
    let items = [
        user("eins"),
        // A response cancelled before its first word, and a transcript that
        // came back empty (a detector false positive).
        assistant(""),
        heard(""),
        item(json!({"type": "message", "role": "system",
                    "content": [{"type": "input_text", "text": "Now speak German."}]})),
        user("zwei"),
    ];
    let got = shape(&messages(&items, "Be brief."));
    assert_eq!(
        got,
        vec![
            (Role::System, s(&["Be brief.\n\nNow speak German."])),
            // With the empty assistant turn gone, the two user turns meet
            // and merge.
            (Role::User, s(&["eins\nzwei"])),
        ]
    );
}

#[test]
fn a_tool_result_directly_follows_its_call() {
    // The user spoke while the tool ran; the result arrived after that.
    let items = [
        user("Wie spät ist es?"),
        assistant("Ich schaue nach."),
        call("call_a", "get_time", r#"{"tz":"Europe/Berlin"}"#),
        user("Und das Wetter?"),
        output("call_a", "12:00"),
    ];
    let ms = messages(&items, "");
    assert_eq!(
        shape(&ms),
        vec![
            (Role::User, s(&["Wie spät ist es?"])),
            // The text the response spoke first and its call are one turn.
            (
                Role::Assistant,
                s(&["Ich schaue nach.", "call:call_a:get_time"])
            ),
            (Role::Tool, s(&["result:call_a:get_time:12:00"])),
            (Role::User, s(&["Und das Wetter?"])),
        ]
    );
    // The arguments reach the model as JSON, not as a string of it.
    match &ms[1].content[1] {
        ContentPart::ToolUse { args, .. } => assert_eq!(args, &json!({"tz": "Europe/Berlin"})),
        other => panic!("{other:?}"),
    }
}

#[test]
fn parallel_calls_share_one_turn_and_a_missing_result_is_filled() {
    let items = [
        user("Zeit und Wetter?"),
        call("call_a", "get_time", "{}"),
        call("call_b", "get_weather", ""),
        output("call_b", "sonnig"),
    ];
    assert_eq!(
        shape(&messages(&items, "")),
        vec![
            (Role::User, s(&["Zeit und Wetter?"])),
            (
                Role::Assistant,
                s(&["call:call_a:get_time", "call:call_b:get_weather"])
            ),
            (
                Role::Tool,
                vec![format!("result:call_a:get_time:{NO_RESULT_YET}")]
            ),
            (Role::Tool, s(&["result:call_b:get_weather:sonnig"])),
        ]
    );
}

#[test]
fn an_output_without_its_call_is_said_as_the_user_s_and_an_unfinished_call_is_not_made() {
    let items = [
        user("los"),
        // Cancelled mid-arguments: never completed, never made.
        item(
            json!({"type": "function_call", "call_id": "call_x", "name": "f",
                    "arguments": "{\"a\":", "status": "incomplete"}),
        ),
        // Its call was deleted, or never existed.
        output("call_gone", "42"),
        output("call_x", "late"),
    ];
    assert_eq!(
        shape(&messages(&items, "")),
        vec![(
            Role::User,
            s(&[
                "los\n[result of function call call_gone]\n42\n[result of function call \
                 call_x]\nlate"
            ])
        )]
    );
}

#[test]
fn an_output_placed_before_its_call_is_pulled_up_to_it() {
    let items = [
        output("call_a", "12:00"),
        user("Zeit?"),
        call("call_a", "get_time", "{}"),
    ];
    assert_eq!(
        shape(&messages(&items, "")),
        vec![
            (Role::User, s(&["Zeit?"])),
            (Role::Assistant, s(&["call:call_a:get_time"])),
            (Role::Tool, s(&["result:call_a:get_time:12:00"])),
        ]
    );
}

#[test]
fn tools_lose_their_schema_keys_and_keep_everything_else() {
    // `@openai/agents`' tool, as captured (agents_js_fc.json).
    let tool: Tool = serde_json::from_value(json!({
        "type": "function", "name": "get_time", "description": "Get the current time",
        "parameters": {"type": "object",
            "properties": {"tz": {"description": "IANA timezone",
                "anyOf": [{"type": "string", "description": "IANA timezone"}, {"type": "null"}]}},
            "required": ["tz"], "additionalProperties": false,
            "$schema": "http://json-schema.org/draft-07/schema#"}
    }))
    .unwrap();
    let choice = serde_json::from_value(json!("required")).unwrap();
    let ir = render(&Input {
        alias: "chatty",
        instructions: "",
        items: &[user("hi")],
        tools: std::slice::from_ref(&tool),
        tool_choice: Some(&choice),
        parallel_tool_calls: Some(false),
        max_output_tokens: Some(MaxOutputTokens::Count(77)),
        reasoning: None,
        speech_hint: None,
        written: &|_| None,
        mcp: Default::default(),
    });
    assert_eq!(ir.tools.len(), 1);
    assert_eq!(
        ir.tools[0].parameters,
        json!({"type": "object",
            "properties": {"tz": {"description": "IANA timezone",
                "anyOf": [{"type": "string", "description": "IANA timezone"}, {"type": "null"}]}},
            "required": ["tz"], "additionalProperties": false})
    );
    assert_eq!(ir.tool_choice, Some(ToolChoice::Required));
    assert_eq!(ir.passthrough["parallel_tool_calls"], json!(false));
    assert_eq!(ir.params.max_tokens, Some(77));
    assert!(ir.stream);
    assert_eq!(ir.model_alias, "chatty");

    // No tools: no tool choice either — some upstreams refuse the pair.
    let ir = render(&Input {
        alias: "chatty",
        instructions: "",
        items: &[user("hi")],
        tools: &[],
        tool_choice: Some(&choice),
        parallel_tool_calls: Some(false),
        max_output_tokens: None,
        reasoning: None,
        speech_hint: None,
        written: &|_| None,
        mcp: Default::default(),
    });
    assert_eq!(ir.tool_choice, None);
    assert!(ir.passthrough.is_empty());
}

#[test]
fn schema_keys_go_at_every_depth_but_only_where_a_schema_stands() {
    let mut schema = json!({
        "$schema": "x",
        "type": "object",
        "properties": {
            // A property that happens to be called `$schema` is the
            // client's.
            "$schema": {"type": "string", "$schema": "nested"},
            "list": {"type": "array", "items": {"$schema": "x", "type": "string"}},
            "pick": {"oneOf": [{"$schema": "x", "type": "integer"}],
                     "default": {"$schema": "data, not a schema"}}
        },
        "$defs": {"thing": {"$schema": "x", "type": "null"}},
        // draft-07: a schema, or a list of property names (data).
        "dependencies": {"a": {"$schema": "x", "required": ["b"]}, "c": ["$schema"]},
        "contentSchema": {"$schema": "x", "type": "object"}
    });
    strip_schema_keys(&mut schema);
    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "$schema": {"type": "string"},
                "list": {"type": "array", "items": {"type": "string"}},
                "pick": {"oneOf": [{"type": "integer"}],
                         "default": {"$schema": "data, not a schema"}}
            },
            "$defs": {"thing": {"type": "null"}},
            "dependencies": {"a": {"required": ["b"]}, "c": ["$schema"]},
            "contentSchema": {"type": "object"}
        })
    );
}

#[test]
fn reasoning_is_off_unless_the_session_names_an_effort() {
    let off = reasoning_control(None);
    assert_eq!(off.enabled, Some(false));
    assert_eq!(reasoning_control(Some(&json!({}))).enabled, Some(false));
    let high = reasoning_control(Some(&json!({"effort": "high"})));
    assert_eq!(high.enabled, Some(true));
    assert_eq!(high.effort.as_deref(), Some("high"));
    let none = reasoning_control(Some(&json!({"effort": "none"})));
    assert_eq!(none.enabled, Some(false));
    assert_eq!(none.effort, None);
}
