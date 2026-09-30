//! The behaviour probes (benchmark design §5): their requests and their
//! verdicts, as pure functions. The runner that sends them is
//! [`super::phases::probes`].
//!
//! Every probe but `thinking_on` switches thinking off
//! (`chat_template_kwargs.enable_thinking = false`) so answers stay short, and
//! every one samples at temperature 0 with the suite's seed. The answer
//! budgets (`max_tokens`) below are part of the suite: they bound the answer
//! the probe asks for, never a prompt, and a budget a model runs out of shows
//! up in the evidence as `finish_reason: length`.

use lmgw_api_types::bench::{ProbeEvidence, ProbeKind, ProbeOutcome, ProbeResult, ServerFacts};
use serde_json::{json, Value};

use super::client::HttpAnswer;

/// The needle probe's passcode.
pub const PASSCODE: &str = "83125947";

/// The reasoning the history probe plants in an earlier assistant turn.
pub const HISTORY_MARKER: &str = "BENCH-REASONING-MARKER-5821";

/// Answer budget of the short probes.
pub const SHORT_ANSWER: u32 = 128;
/// `thinking_on`'s budget: reasoning needs room, and running out of it still
/// leaves the reasoning that was produced to judge.
pub const THINKING_ANSWER: u32 = 1024;
/// `tool_call` and `json_schema`: a call or an object, with its syntax.
pub const STRUCTURED_ANSWER: u32 = 256;

/// The row's derived capabilities, as far as the probes care (§5
/// "Capabilities"). `None` is "unknown": the probe runs and its result is
/// the answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub reasoning: Option<bool>,
    pub tools: Option<bool>,
    /// The row has a projector (`--mmproj`).
    pub projector: bool,
}

/// Why a probe does not apply, or `None` when it runs. The live
/// `/props` `chat_template_caps.supports_tools` wins over the row's claim
/// where the engine reports it.
pub fn skip_reason(
    kind: ProbeKind,
    caps: Caps,
    server: &ServerFacts,
    needle_tokens: Option<u64>,
) -> Option<String> {
    match kind {
        ProbeKind::ThinkingOff | ProbeKind::ThinkingOn if caps.reasoning == Some(false) => {
            Some("the row's capabilities say the model does not reason".into())
        }
        ProbeKind::ToolCall => {
            let live = server.chat_template_caps.get("supports_tools").copied();
            match (live, caps.tools) {
                (Some(false), _) => {
                    Some("the server's chat template reports no tool support".into())
                }
                (None, Some(false)) => {
                    Some("the row's capabilities say the template has no tool support".into())
                }
                _ => None,
            }
        }
        ProbeKind::Vision if !caps.projector => Some("no projector".into()),
        ProbeKind::Needle if needle_tokens.is_none() => Some(format!(
            "the per-slot context ({} tokens) leaves no room for a haystack",
            server.per_slot_ctx
        )),
        _ => None,
    }
}

fn chat(messages: Value, max_tokens: u32, thinking: bool, seed: u64) -> Value {
    json!({
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": 0,
        "seed": seed,
        "stream": false,
        "chat_template_kwargs": { "enable_thinking": thinking },
    })
}

fn user(text: &str) -> Value {
    json!([{ "role": "user", "content": text }])
}

pub fn chat_request(seed: u64) -> Value {
    chat(
        user("In one short sentence: what is the capital of France?"),
        SHORT_ANSWER,
        false,
        seed,
    )
}

pub fn thinking_request(on: bool, seed: u64) -> Value {
    chat(
        user("What is 17 + 25? Answer with the number only."),
        if on { THINKING_ANSWER } else { SHORT_ANSWER },
        on,
        seed,
    )
}

pub fn tool_request(seed: u64) -> Value {
    let mut body = chat(
        user("What is the weather in Paris right now? Use the tool."),
        STRUCTURED_ANSWER,
        false,
        seed,
    );
    body["tools"] = json!([{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get the current weather for a city.",
            "parameters": {
                "type": "object",
                "properties": { "city": { "type": "string", "description": "The city name" } },
                "required": ["city"]
            }
        }
    }]);
    body["tool_choice"] = json!("auto");
    body
}

pub fn json_schema_request(seed: u64) -> Value {
    let mut body = chat(
        user("Alice is 30 years old. Return her as a JSON object with her name and age."),
        STRUCTURED_ANSWER,
        false,
        seed,
    );
    body["response_format"] = json!({
        "type": "json_schema",
        "json_schema": {
            "name": "person",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "age": { "type": "integer" }
                },
                "required": ["name", "age"],
                "additionalProperties": false
            }
        }
    });
    body
}

pub fn deterministic_request(seed: u64) -> Value {
    chat(
        user("Write one sentence about the sea."),
        SHORT_ANSWER,
        false,
        seed,
    )
}

/// The same request, with `cache_prompt: false`. Live testing (§2.1) found
/// prompt-cache reuse is not bit-deterministic on some rows: this pair
/// isolates whether that is the cause by forcing every send to compute the
/// prompt fresh.
pub fn deterministic_request_no_cache(seed: u64) -> Value {
    let mut body = deterministic_request(seed);
    body["cache_prompt"] = json!(false);
    body
}

pub fn vision_request(image_data_url: &str, seed: u64) -> Value {
    chat(
        json!([{
            "role": "user",
            "content": [
                { "type": "text", "text": "What colour is this image? Answer with one word." },
                { "type": "image_url", "image_url": { "url": image_data_url } }
            ]
        }]),
        SHORT_ANSWER,
        false,
        seed,
    )
}

/// `/apply-template` body: an earlier assistant turn carrying
/// `reasoning_content`, then a new user turn. Thinking on, since that is
/// when a kept reasoning history matters.
pub fn history_template_request() -> Value {
    json!({
        "messages": [
            { "role": "user", "content": "What is 2 + 2?" },
            { "role": "assistant", "content": "4", "reasoning_content": HISTORY_MARKER },
            { "role": "user", "content": "And 3 + 3?" }
        ],
        "chat_template_kwargs": { "enable_thinking": true },
    })
}

pub fn needle_request(haystack: &str, seed: u64) -> Value {
    let text = format!(
        "The passcode is {PASSCODE}. Remember it; you will be asked for it.\n\n{haystack}\n\n\
         What is the passcode given at the very beginning of this message? \
         Reply with the passcode only."
    );
    chat(user(&text), SHORT_ANSWER, false, seed)
}

/// A chat answer, read for the verdicts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatReading {
    pub content: String,
    pub reasoning: String,
    pub finish_reason: Option<String>,
    pub tool_calls: Option<Value>,
}

/// The evidence of one answer, and its reading when it is a 200 chat
/// completion. `Err` carries an [`ProbeOutcome::Error`] result's detail.
pub fn read_chat(ans: &HttpAnswer) -> (ProbeEvidence, Result<ChatReading, String>) {
    let mut ev = ProbeEvidence {
        status: Some(ans.status),
        ms: Some(ans.ms),
        ..Default::default()
    };
    if !ans.ok() {
        ev.body = Some(ans.body.clone());
        return (ev, Err(format!("HTTP {}", ans.status)));
    }
    let Some(v) = ans.json() else {
        ev.body = Some(ans.body.clone());
        return (ev, Err("the 200 answer is not JSON".into()));
    };
    let Some(msg) = v.pointer("/choices/0/message") else {
        ev.body = Some(ans.body.clone());
        return (ev, Err("the answer has no choices[0].message".into()));
    };
    let s = |k: &str| msg.get(k).and_then(Value::as_str).map(str::to_string);
    let reading = ChatReading {
        content: s("content").unwrap_or_default(),
        reasoning: s("reasoning_content").unwrap_or_default(),
        finish_reason: v
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        tool_calls: msg.get("tool_calls").filter(|t| !t.is_null()).cloned(),
    };
    ev.content = s("content");
    ev.reasoning = s("reasoning_content");
    ev.finish_reason = reading.finish_reason.clone();
    ev.tool_calls = reading.tool_calls.clone();
    (ev, Ok(reading))
}

/// A transport failure (no status at all) as a probe result.
pub fn transport_error(kind: ProbeKind, message: &str) -> ProbeResult {
    ProbeResult {
        probe: kind,
        outcome: ProbeOutcome::Error,
        detail: message.to_string(),
        evidence: vec![ProbeEvidence::default()],
    }
}

fn result(
    kind: ProbeKind,
    outcome: ProbeOutcome,
    detail: impl Into<String>,
    ev: ProbeEvidence,
) -> ProbeResult {
    ProbeResult {
        probe: kind,
        outcome,
        detail: detail.into(),
        evidence: vec![ev],
    }
}

/// Judge one chat answer with `verdict` over its reading; non-200s and
/// unreadable bodies are `error`.
fn judge(
    kind: ProbeKind,
    ans: &HttpAnswer,
    verdict: impl FnOnce(&ChatReading) -> (ProbeOutcome, String),
) -> ProbeResult {
    let (ev, reading) = read_chat(ans);
    match reading {
        Err(detail) => result(kind, ProbeOutcome::Error, detail, ev),
        Ok(r) => {
            let (outcome, detail) = verdict(&r);
            result(kind, outcome, detail, ev)
        }
    }
}

fn pass(detail: impl Into<String>) -> (ProbeOutcome, String) {
    (ProbeOutcome::Pass, detail.into())
}

fn fail(detail: impl Into<String>) -> (ProbeOutcome, String) {
    (ProbeOutcome::Fail, detail.into())
}

/// The text of a `<think>…</think>` block in `content`, when there is one —
/// reasoning the server did not parse out. A template that opens the block
/// in the prompt leaves only the closing tag in the answer, so text before a
/// lone `</think>` counts too, and so does an unclosed `<think>`.
fn think_block(content: &str) -> Option<&str> {
    const OPEN: &str = "<think>";
    let start = content.find(OPEN).map(|s| s + OPEN.len());
    let end = content.find("</think>");
    match (start, end) {
        (Some(s), Some(e)) if e >= s => Some(content[s..e].trim()),
        (Some(s), _) => Some(content[s..].trim()),
        (None, Some(e)) => Some(content[..e].trim()),
        (None, None) => None,
    }
}

/// `content` without its think block: what follows `</think>`, nothing for
/// an unclosed `<think>`, else all of it.
fn answer_text(content: &str) -> &str {
    match content.find("</think>") {
        Some(e) => content[e + "</think>".len()..].trim(),
        None if content.contains("<think>") => "",
        None => content.trim(),
    }
}

pub fn judge_chat(ans: &HttpAnswer) -> ProbeResult {
    judge(ProbeKind::Chat, ans, |r| {
        match (r.content.trim().is_empty(), r.finish_reason.as_deref()) {
            (false, Some("stop")) => pass("answered, finish_reason stop"),
            (true, _) => fail("empty content"),
            (false, other) => fail(format!("finish_reason {other:?}, expected \"stop\"")),
        }
    })
}

pub fn judge_thinking_off(ans: &HttpAnswer) -> ProbeResult {
    judge(ProbeKind::ThinkingOff, ans, |r| {
        if !r.reasoning.trim().is_empty() {
            return fail("reasoning_content is present although thinking was switched off");
        }
        if think_block(&r.content).is_some_and(|t| !t.is_empty()) {
            return fail("the content carries a non-empty <think> block");
        }
        if answer_text(&r.content).is_empty() {
            return fail("empty content");
        }
        pass("no reasoning, content present")
    })
}

pub fn judge_thinking_on(ans: &HttpAnswer) -> ProbeResult {
    judge(ProbeKind::ThinkingOn, ans, |r| {
        if !r.reasoning.trim().is_empty() {
            return pass("reasoning_content present");
        }
        if think_block(&r.content).is_some_and(|t| !t.is_empty()) {
            return fail(
                "the reasoning arrived inside content, not as reasoning_content (no reasoning \
                 parser for this template?)",
            );
        }
        fail("no reasoning_content although thinking was switched on")
    })
}

pub fn judge_tool_call(ans: &HttpAnswer) -> ProbeResult {
    judge(ProbeKind::ToolCall, ans, |r| {
        let Some(calls) = r.tool_calls.as_ref().and_then(Value::as_array) else {
            return fail("no tool_calls in the answer");
        };
        if calls.len() != 1 {
            return fail(format!("{} tool calls, expected one", calls.len()));
        }
        let f = &calls[0]["function"];
        if f["name"].as_str() != Some("get_weather") {
            return fail(format!("called {}, expected get_weather", f["name"]));
        }
        // OpenAI's shape is a JSON *string*; accept an object too.
        let args = match &f["arguments"] {
            Value::String(s) => serde_json::from_str::<Value>(s).ok(),
            v @ Value::Object(_) => Some(v.clone()),
            _ => None,
        };
        match args {
            Some(Value::Object(m)) if m.get("city").is_some_and(Value::is_string) => {
                pass("one get_weather call with a string city")
            }
            Some(Value::Object(_)) => fail("the arguments have no string 'city'"),
            _ => fail("the arguments do not parse as a JSON object"),
        }
    })
}

pub fn judge_json_schema(ans: &HttpAnswer) -> ProbeResult {
    judge(ProbeKind::JsonSchema, ans, |r| {
        let Ok(v) = serde_json::from_str::<Value>(answer_text(&r.content)) else {
            return fail("the content does not parse as JSON");
        };
        match (v.get("name"), v.get("age")) {
            (Some(Value::String(_)), Some(age)) if age.is_i64() || age.is_u64() => {
                pass("an object with a string name and an integer age")
            }
            _ => fail("the object lacks a string 'name' or an integer 'age'"),
        }
    })
}

/// Two pairs of identical temperature-0, seeded requests: `cached` as a
/// client would send them (prompt-cache reuse allowed — the default), and
/// `no_cache` with `cache_prompt: false`. Live testing (§2.1) found some rows
/// give two distinct answers when the cache is reused and identical answers
/// without it — a reused prefix is computed in a different batch shape, so
/// the numerics differ.
///
/// Verdict: the no-cache pair differing is a `fail` (not reproducible even
/// without the cache); the no-cache pair identical but the cached pair
/// differing is `info` (reproducible only without prompt-cache reuse); all
/// four identical is `pass`.
pub fn judge_deterministic(
    cached: (&HttpAnswer, &HttpAnswer),
    no_cache: (&HttpAnswer, &HttpAnswer),
) -> ProbeResult {
    let (ev_ca, ra) = read_chat(cached.0);
    let (ev_cb, rb) = read_chat(cached.1);
    let (ev_na, rc) = read_chat(no_cache.0);
    let (ev_nb, rd) = read_chat(no_cache.1);
    let evidence = vec![ev_ca, ev_cb, ev_na, ev_nb];
    let (outcome, detail) = match (ra, rb, rc, rd) {
        (Err(e), ..) | (_, Err(e), _, _) | (_, _, Err(e), _) | (_, _, _, Err(e)) => {
            (ProbeOutcome::Error, e)
        }
        (Ok(a), Ok(b), Ok(c), Ok(d)) => {
            let any_empty = [&a, &b, &c, &d].iter().any(|r| r.content.trim().is_empty());
            if any_empty {
                (ProbeOutcome::Fail, "empty content".to_string())
            } else if c.content != d.content {
                (
                    ProbeOutcome::Fail,
                    "the two answers differ even with cache_prompt: false".to_string(),
                )
            } else if a.content != b.content {
                (
                    ProbeOutcome::Info,
                    "the output is reproducible only without prompt-cache reuse: a reused \
                     prefix is computed in a different batch shape, so the numerics differ"
                        .to_string(),
                )
            } else {
                (
                    ProbeOutcome::Pass,
                    "all four answers are identical".to_string(),
                )
            }
        }
    };
    ProbeResult {
        probe: ProbeKind::Deterministic,
        outcome,
        detail,
        evidence,
    }
}

/// Words that name the probe image's colour: red, or a red a model may
/// choose instead.
const RED_WORDS: [&str; 4] = ["red", "reddish", "crimson", "scarlet"];

/// Words that negate the word right after them ("not red", "isn't red").
fn negates(word: &str) -> bool {
    // "isn't", "wasn't", "doesn't", … and the same typed without the
    // apostrophe.
    word.ends_with("n't")
        || matches!(
            word,
            "not"
                | "no"
                | "never"
                | "nor"
                | "neither"
                | "isnt"
                | "arent"
                | "wasnt"
                | "doesnt"
                | "dont"
        )
}

/// The red word the answer names, if any: its words (letters and
/// apostrophes, lowercased) are read one by one, so "colored", "covered" or
/// "rendered" do not count, and a red word right after a negation ("not
/// red") does not either. `Err` carries the negated mention, for the detail.
fn names_red(text: &str) -> Result<String, Option<String>> {
    let lower = text.to_lowercase().replace('’', "'");
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphabetic() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .collect();
    let mut negated = None;
    for (i, w) in words.iter().enumerate() {
        if !RED_WORDS.contains(w) {
            continue;
        }
        match i.checked_sub(1).map(|p| words[p]) {
            Some(prev) if negates(prev) => {
                negated.get_or_insert_with(|| format!("{prev} {w}"));
            }
            _ => return Ok((*w).to_string()),
        }
    }
    Err(negated)
}

pub fn judge_vision(ans: &HttpAnswer) -> ProbeResult {
    judge(ProbeKind::Vision, ans, |r| {
        match names_red(answer_text(&r.content)) {
            Ok(word) => pass(format!("the answer names red (\"{word}\")")),
            Err(Some(neg)) => fail(format!("the answer denies red (\"{neg}\")")),
            Err(None) => fail("the answer does not name red"),
        }
    })
}

/// `info` either way: whether the marker survived the template.
pub fn judge_reasoning_history(ans: &HttpAnswer) -> ProbeResult {
    let ev = ProbeEvidence {
        status: Some(ans.status),
        ms: Some(ans.ms),
        body: Some(ans.body.clone()),
        ..Default::default()
    };
    if !ans.ok() {
        return result(
            ProbeKind::ReasoningHistory,
            ProbeOutcome::Error,
            format!("POST /apply-template: HTTP {}", ans.status),
            ev,
        );
    }
    let Some(prompt) = ans
        .json()
        .and_then(|v| v["prompt"].as_str().map(str::to_string))
    else {
        return result(
            ProbeKind::ReasoningHistory,
            ProbeOutcome::Error,
            "POST /apply-template: no 'prompt' in the answer",
            ev,
        );
    };
    let detail = if prompt.contains(HISTORY_MARKER) {
        "kept: the template renders an earlier assistant turn's reasoning_content"
    } else {
        "dropped: the template does not render an earlier assistant turn's reasoning_content"
    };
    let ev = ProbeEvidence {
        body: Some(prompt),
        ..ev
    };
    result(ProbeKind::ReasoningHistory, ProbeOutcome::Info, detail, ev)
}

pub fn judge_needle(ans: &HttpAnswer) -> ProbeResult {
    judge(ProbeKind::Needle, ans, |r| {
        let digits: String = r.content.chars().filter(char::is_ascii_digit).collect();
        if digits.contains(PASSCODE) {
            pass("the answer contains the passcode")
        } else {
            fail("the answer does not contain the passcode")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(body: Value) -> HttpAnswer {
        HttpAnswer {
            status: 200,
            body: body.to_string(),
            ms: 12,
        }
    }

    fn msg(content: &str, reasoning: Option<&str>, finish: &str) -> HttpAnswer {
        let mut m = json!({ "role": "assistant", "content": content });
        if let Some(r) = reasoning {
            m["reasoning_content"] = json!(r);
        }
        ok(json!({ "choices": [{ "message": m, "finish_reason": finish }] }))
    }

    fn err(status: u16, body: &str) -> HttpAnswer {
        HttpAnswer {
            status,
            body: body.into(),
            ms: 3,
        }
    }

    #[test]
    fn chat_verdicts() {
        let r = judge_chat(&msg("Paris.", None, "stop"));
        assert_eq!(r.outcome, ProbeOutcome::Pass);
        assert_eq!(r.evidence[0].content.as_deref(), Some("Paris."));
        assert_eq!(r.evidence[0].status, Some(200));
        assert_eq!(
            judge_chat(&msg("  ", None, "stop")).outcome,
            ProbeOutcome::Fail
        );
        assert_eq!(
            judge_chat(&msg("Par", None, "length")).outcome,
            ProbeOutcome::Fail
        );
        let e = judge_chat(&err(500, "{\"error\":\"boom\"}"));
        assert_eq!(e.outcome, ProbeOutcome::Error);
        assert_eq!(e.evidence[0].body.as_deref(), Some("{\"error\":\"boom\"}"));
        assert_eq!(
            judge_chat(&err(200, "not json")).outcome,
            ProbeOutcome::Error
        );
    }

    #[test]
    fn thinking_verdicts() {
        use ProbeOutcome::*;
        assert_eq!(judge_thinking_off(&msg("42", None, "stop")).outcome, Pass);
        assert_eq!(
            judge_thinking_off(&msg("42", Some(""), "stop")).outcome,
            Pass
        );
        // ik without a reasoning parser: an empty think block is harmless…
        assert_eq!(
            judge_thinking_off(&msg("<think>\n\n</think>\n\n42", None, "stop")).outcome,
            Pass
        );
        // …a full one is reasoning that leaked into the content.
        assert_eq!(
            judge_thinking_off(&msg("<think>17+25…</think>42", None, "stop")).outcome,
            Fail
        );
        assert_eq!(
            judge_thinking_off(&msg("42", Some("hmm"), "stop")).outcome,
            Fail
        );
        assert_eq!(judge_thinking_off(&msg("", None, "stop")).outcome, Fail);

        assert_eq!(
            judge_thinking_on(&msg("42", Some("17+25=42"), "stop")).outcome,
            Pass
        );
        assert_eq!(
            judge_thinking_on(&msg("", Some("still thinking"), "length")).outcome,
            Pass
        );
        let r = judge_thinking_on(&msg("<think>17+25</think>42", None, "stop"));
        assert_eq!(r.outcome, Fail);
        assert!(r.detail.contains("inside content"), "{}", r.detail);
        assert_eq!(judge_thinking_on(&msg("42", None, "stop")).outcome, Fail);
        // The template opened the block in the prompt: only `</think>` is left.
        let r = judge_thinking_on(&msg("17+25 is 42.\n</think>\n\n42", None, "stop"));
        assert!(r.detail.contains("inside content"), "{}", r.detail);
        assert_eq!(
            judge_thinking_off(&msg("\n</think>\n\n42", None, "stop")).outcome,
            Pass
        );
    }

    #[test]
    fn tool_call_verdicts() {
        use ProbeOutcome::*;
        let call = |name: &str, args: Value| {
            ok(
                json!({ "choices": [{ "finish_reason": "tool_calls", "message": {
                    "role": "assistant", "content": "",
                    "tool_calls": [{ "type": "function", "id": "x",
                        "function": { "name": name, "arguments": args } }]
                }}]}),
            )
        };
        assert_eq!(
            judge_tool_call(&call("get_weather", json!("{\"city\":\"Paris\"}"))).outcome,
            Pass
        );
        assert_eq!(
            judge_tool_call(&call("get_weather", json!({"city": "Paris"}))).outcome,
            Pass
        );
        assert_eq!(
            judge_tool_call(&call("get_weather", json!("{\"town\":\"Paris\"}"))).outcome,
            Fail
        );
        assert_eq!(
            judge_tool_call(&call("get_weather", json!("not json"))).outcome,
            Fail
        );
        assert_eq!(
            judge_tool_call(&call("get_time", json!("{\"city\":\"Paris\"}"))).outcome,
            Fail
        );
        assert_eq!(
            judge_tool_call(&msg("It is sunny.", None, "stop")).outcome,
            Fail
        );
        let r = judge_tool_call(&call("get_weather", json!("{\"city\":\"Paris\"}")));
        assert!(r.evidence[0].tool_calls.is_some());
    }

    #[test]
    fn json_schema_verdicts() {
        use ProbeOutcome::*;
        assert_eq!(
            judge_json_schema(&msg(
                "{\n  \"name\": \"Alice\",\n  \"age\": 30\n}",
                None,
                "stop"
            ))
            .outcome,
            Pass
        );
        assert_eq!(
            judge_json_schema(&msg("{\"name\":\"Alice\",\"age\":\"30\"}", None, "stop")).outcome,
            Fail
        );
        assert_eq!(
            judge_json_schema(&msg("{\"name\":\"Alice\",\"age\":30.5}", None, "stop")).outcome,
            Fail
        );
        assert_eq!(
            judge_json_schema(&msg("Alice, 30", None, "stop")).outcome,
            Fail
        );
    }

    #[test]
    fn deterministic_verdicts() {
        use ProbeOutcome::*;
        let a = msg("The sea is wide.", None, "stop");
        let b = msg("The sea is deep.", None, "stop");
        let same = msg("The sea is calm.", None, "stop");

        // All four identical → pass.
        let r = judge_deterministic((&a, &a), (&a, &a));
        assert_eq!(r.outcome, Pass);
        assert_eq!(r.evidence.len(), 4);

        // The no-cache pair identical, the cached pair differs → info,
        // naming prompt-cache reuse, with both cached answers in evidence.
        let r = judge_deterministic((&a, &b), (&same, &same));
        assert_eq!(r.outcome, Info);
        assert!(r.detail.contains("prompt-cache reuse"), "{}", r.detail);
        assert_eq!(r.evidence[0].content.as_deref(), Some("The sea is wide."));
        assert_eq!(r.evidence[1].content.as_deref(), Some("The sea is deep."));

        // The no-cache pair differs → fail, whatever the cached pair says.
        assert_eq!(judge_deterministic((&a, &a), (&a, &b)).outcome, Fail);
        assert_eq!(judge_deterministic((&a, &b), (&a, &b)).outcome, Fail);

        // An error in any of the four answers the whole probe.
        assert_eq!(
            judge_deterministic((&a, &err(503, "busy")), (&a, &a)).outcome,
            Error
        );
        assert_eq!(
            judge_deterministic((&a, &a), (&a, &err(503, "busy"))).outcome,
            Error
        );
        let empty = msg("", None, "stop");
        assert_eq!(
            judge_deterministic((&empty, &empty), (&empty, &empty)).outcome,
            Fail
        );
    }

    #[test]
    fn vision_and_needle_verdicts() {
        use ProbeOutcome::*;
        assert_eq!(judge_vision(&msg("Red.", None, "stop")).outcome, Pass);
        assert_eq!(judge_vision(&msg("Blue", None, "stop")).outcome, Fail);
        assert_eq!(judge_needle(&msg("83125947", None, "stop")).outcome, Pass);
        assert_eq!(
            judge_needle(&msg("The passcode is 83,125,947.", None, "stop")).outcome,
            Pass
        );
        assert_eq!(
            judge_needle(&msg("I don't know.", None, "stop")).outcome,
            Fail
        );
    }

    /// Review finding 2: a substring match passed "colored", "covered",
    /// "rendered" and "not red". Words are read whole, a negation right
    /// before the red word fails it, and the detail names the word matched.
    #[test]
    fn vision_reads_whole_words_and_negations() {
        use ProbeOutcome::*;
        let v = |s: &str| judge_vision(&msg(s, None, "stop"));
        for s in [
            "The image is colored.",
            "It is covered in one flat colour.",
            "A rendered square.",
            "Credit: a blue square",
            "not red",
            "It isn't red, it's blue.",
            "It is NOT RED.",
            "No red here.",
            "Isn’t red.",
        ] {
            assert_eq!(v(s).outcome, Fail, "{s}");
        }
        for (s, word) in [
            ("Red.", "red"),
            ("reddish", "reddish"),
            ("Crimson!", "crimson"),
            ("It is scarlet.", "scarlet"),
            ("Not blue — red.", "red"),
            ("It's not blue, it is red", "red"),
            ("<think>not red?</think>Red", "red"),
        ] {
            let r = v(s);
            assert_eq!(r.outcome, Pass, "{s}: {}", r.detail);
            assert!(r.detail.contains(&format!("\"{word}\"")), "{}", r.detail);
        }
        let r = v("It is not red.");
        assert!(r.detail.contains("\"not red\""), "{}", r.detail);
        assert_eq!(v("Blue").detail, "the answer does not name red");
    }

    #[test]
    fn history_is_information_or_an_error() {
        let kept = ok(
            json!({ "prompt": format!("<|im_start|>assistant\n<think>{HISTORY_MARKER}</think>4") }),
        );
        let r = judge_reasoning_history(&kept);
        assert_eq!(r.outcome, ProbeOutcome::Info);
        assert!(r.detail.starts_with("kept"));
        let dropped = ok(json!({ "prompt": "<|im_start|>assistant\n4<|im_end|>" }));
        let r = judge_reasoning_history(&dropped);
        assert_eq!(r.outcome, ProbeOutcome::Info);
        assert!(r.detail.starts_with("dropped"));
        assert_eq!(
            r.evidence[0].body.as_deref(),
            Some("<|im_start|>assistant\n4<|im_end|>")
        );
        let r = judge_reasoning_history(&err(500, "{\"error\":{\"message\":\"template\"}}"));
        assert_eq!(r.outcome, ProbeOutcome::Error);
        assert_eq!(r.evidence[0].status, Some(500));
        assert!(r.evidence[0].body.as_deref().unwrap().contains("template"));
    }

    #[test]
    fn skips_follow_the_capabilities_and_the_live_template() {
        let mut server = ServerFacts {
            per_slot_ctx: 4096,
            ..Default::default()
        };
        let caps = Caps {
            reasoning: Some(false),
            tools: Some(true),
            projector: false,
        };
        assert!(skip_reason(ProbeKind::ThinkingOn, caps, &server, Some(1)).is_some());
        assert!(skip_reason(ProbeKind::Vision, caps, &server, Some(1)).is_some());
        assert!(skip_reason(ProbeKind::ToolCall, caps, &server, Some(1)).is_none());
        server
            .chat_template_caps
            .insert("supports_tools".into(), false);
        assert!(skip_reason(ProbeKind::ToolCall, caps, &server, Some(1)).is_some());
        let unknown = Caps::default();
        server.chat_template_caps.clear();
        for k in [
            ProbeKind::ThinkingOn,
            ProbeKind::ThinkingOff,
            ProbeKind::ToolCall,
            ProbeKind::Chat,
        ] {
            assert!(skip_reason(k, unknown, &server, Some(1)).is_none(), "{k:?}");
        }
        assert!(skip_reason(ProbeKind::Needle, unknown, &server, None).is_some());
    }

    #[test]
    fn requests_carry_the_pinned_knobs() {
        let b = thinking_request(true, 1234);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(b["temperature"], 0);
        assert_eq!(b["seed"], 1234);
        assert_eq!(
            chat_request(1)["chat_template_kwargs"]["enable_thinking"],
            false
        );
        assert_eq!(
            tool_request(1)["tools"][0]["function"]["name"],
            "get_weather"
        );
        assert_eq!(
            json_schema_request(1)["response_format"]["type"],
            "json_schema"
        );
        let det = deterministic_request(1);
        let det_nc = deterministic_request_no_cache(1);
        assert!(det.get("cache_prompt").is_none(), "{det}");
        assert_eq!(det_nc["cache_prompt"], false);
        assert_eq!(det["messages"], det_nc["messages"]);
        assert_eq!(det["seed"], det_nc["seed"]);
        let v = vision_request("data:image/png;base64,AAAA", 1);
        assert_eq!(
            v["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
        let n = needle_request("haystack", 1);
        let text = n["messages"][0]["content"].as_str().unwrap();
        assert!(text.starts_with(&format!("The passcode is {PASSCODE}.")));
        assert!(text.contains("haystack"));
        assert_eq!(
            history_template_request()["messages"][1]["reasoning_content"],
            HISTORY_MARKER
        );
    }
}
