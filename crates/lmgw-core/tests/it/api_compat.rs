//! The compatibility check's own tests (api-compat design, level 1): that
//! it catches what it exists for, that its rules are the documented ones,
//! and that a violation really fails the test process. The check itself is
//! `common::api_compat`; every other suite runs it without asking.

mod conversations;
mod ir_props;

use lmgw_core::config::Protocol;
use lmgw_core::egress::watch::Built;
use serde_json::{json, Value};

use crate::common::api_compat::{egress, known};

/// `body` checked; `client_keys` are the body's keys the client sent and
/// lmgw forwarded verbatim.
fn check(protocol: Protocol, url: &str, body: &Value, client_keys: &[&str]) -> Vec<String> {
    let passthrough: serde_json::Map<String, Value> = client_keys
        .iter()
        .map(|k| (k.to_string(), body[*k].clone()))
        .collect();
    check_with(protocol, url, body, &passthrough)
}

fn check_with(
    protocol: Protocol,
    url: &str,
    body: &Value,
    passthrough: &serde_json::Map<String, Value>,
) -> Vec<String> {
    let bytes = serde_json::to_vec(body).unwrap();
    let out = egress::check(&Built {
        protocol,
        url,
        body: &bytes,
        passthrough,
    });
    assert!(out.entry.is_some(), "no entry point for {url}");
    out.violations.iter().map(ToString::to_string).collect()
}

fn anthropic_turns(input: Value) -> Value {
    json!({
        "model": "claude-test",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "what time is it?"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "now", "input": input}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "12:00"}
            ]}
        ]
    })
}

/// The bug that motivated the check (d3fa11d1): a stored call with no
/// arguments replayed as `"input": ""`, which Anthropic refused with
/// "tool_use.input: Input should be an object".
#[test]
fn an_anthropic_tool_use_with_string_input_is_a_violation() {
    let url = "https://api.anthropic.test/v1/messages";
    let bad = check(Protocol::Anthropic, url, &anthropic_turns(json!("")), &[]);
    assert_eq!(bad.len(), 1, "{bad:#?}");
    assert!(
        bad[0].starts_with("/messages/1/content/0/input: "),
        "the report names the field: {bad:#?}"
    );
    assert!(check(Protocol::Anthropic, url, &anthropic_turns(json!({})), &[]).is_empty());
}

/// A model's broken arguments replayed as their text are a known violation
/// (`known::KNOWN`), printed but not failing; a blank string never is.
#[test]
fn unparseable_arguments_are_known_and_blank_ones_are_not() {
    let body = serde_json::to_vec(&anthropic_turns(json!("{\"city\": \"Ber"))).unwrap();
    let out = egress::check(&Built {
        protocol: Protocol::Anthropic,
        url: "https://api.anthropic.test/v1/messages",
        body: &body,
        passthrough: &serde_json::Map::new(),
    });
    assert!(out.violations.is_empty(), "{:?}", out.violations);
    assert_eq!(out.known.len(), 1, "{:?}", out.known);
    let blank = check(
        Protocol::Anthropic,
        "https://api.anthropic.test/v1/messages",
        &anthropic_turns(json!("  ")),
        &[],
    );
    assert_eq!(blank.len(), 1, "{blank:#?}");
}

/// The request side is strict: an unknown key is a violation, as the
/// provider refuses it.
#[test]
fn an_unknown_key_is_a_violation_unless_the_client_sent_it() {
    let url = "https://api.openai.test/v1/chat/completions";
    let body = json!({
        "model": "gpt-test",
        "messages": [{"role": "user", "content": "hi"}],
        "grammar": "root ::= \"x\"",
    });
    let bad = check(Protocol::Openai, url, &body, &[]);
    assert_eq!(bad.len(), 1, "{bad:#?}");
    assert!(bad[0].contains("grammar"), "{bad:#?}");
    // The client's own key, forwarded as sent, is the client's.
    assert!(check(Protocol::Openai, url, &body, &["grammar"]).is_empty());
}

/// llama.cpp's extensions are allowed on the rows they are sent to, and
/// only there: the same body to the OpenAI platform names them.
#[test]
fn llama_extensions_pass_on_llama_rows_only() {
    let url = "http://llama.test/v1/chat/completions";
    let body = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello", "reasoning_content": "greet back"},
            {"role": "user", "content": "again"}
        ],
        "top_k": 40,
        "min_p": 0.05,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    assert!(check(Protocol::LlamaCpp, url, &body, &[]).is_empty());
    let on_platform = check(Protocol::Openai, url, &body, &[]);
    assert_eq!(on_platform.len(), 2, "{on_platform:#?}");
}

/// Gemini's request, through the converted discovery document: a function
/// call's `args` must be an object there too.
#[test]
fn a_gemini_function_call_with_string_args_is_a_violation() {
    let url = "https://gemini.test/v1beta/models/m:generateContent";
    let body = |args: Value| {
        json!({
            "contents": [
                {"role": "user", "parts": [{"text": "time?"}]},
                {"role": "model", "parts": [{"functionCall": {"name": "now", "args": args}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "now", "response": {"t": "12:00"}}}]}
            ]
        })
    };
    let bad = check(Protocol::Gemini, url, &body(json!("")), &[]);
    assert_eq!(bad.len(), 1, "{bad:#?}");
    assert!(
        bad[0].starts_with("/contents/1/parts/0/functionCall/args"),
        "{bad:#?}"
    );
    assert!(check(Protocol::Gemini, url, &body(json!({})), &[]).is_empty());
}

/// An MP3's registered type is `audio/mpeg`; OpenAI's `input_audio.format`
/// takes `mp3` (and `wav`), never `mpeg` (found by this check).
#[test]
fn audio_mpeg_goes_as_input_audio_format_mp3() {
    use lmgw_core::ir::{ChatRequest, ContentPart, Message, Role};
    let up = crate::egress_golden::generic_upstream();
    let mut ir: ChatRequest = serde_json::from_value(json!({
        "model_alias": "m",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "transcribe"}]}],
    }))
    .unwrap();
    for mime in [
        "audio/mpeg",
        "audio/MPEG",
        "audio/x-mp3",
        "audio/x-wav",
        "audio/Wave",
    ] {
        ir.messages = vec![Message {
            role: Role::User,
            content: vec![ContentPart::Audio {
                mime: mime.into(),
                data: "SUQz".into(),
            }],
        }];
        let req = lmgw_core::egress::for_protocol(up.protocol)
            .build_chat(&reqwest::Client::new(), &up, "m", &ir, &ir.params, false)
            .unwrap()
            .build()
            .unwrap();
        let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        let format = &body["messages"][0]["content"][0]["input_audio"]["format"];
        assert_eq!(
            format,
            if mime.to_ascii_lowercase().contains("wav") {
                "wav"
            } else {
                "mp3"
            },
            "{body}"
        );
        assert!(check(up.protocol, req.url().as_str(), &body, &[]).is_empty());
    }
}

/// A client's key forwarded verbatim is the client's to answer for; a key
/// lmgw wrote into (the `reasoning` object it reconciles) is lmgw's, checked.
#[test]
fn a_passthrough_key_lmgw_rewrote_is_checked() {
    let url = "https://api.openai.test/v1/responses";
    let sent = json!({"effort": "bogus"});
    let passthrough: serde_json::Map<String, Value> = [("reasoning".to_string(), sent.clone())]
        .into_iter()
        .collect();
    let body = |r: Value| json!({"model": "m", "input": "hi", "reasoning": r});
    assert!(check_with(Protocol::Openai, url, &body(sent), &passthrough).is_empty());
    let rewritten = check_with(
        Protocol::Openai,
        url,
        &body(json!({"effort": "bogus2"})),
        &passthrough,
    );
    assert!(
        rewritten.iter().any(|l| l.starts_with("/reasoning/effort")),
        "{rewritten:#?}"
    );
}

/// A panic inside `quietly` does not leave the thread quiet.
#[test]
fn quietly_puts_the_flag_back_after_a_panic() {
    use crate::common::api_compat::ledger;
    assert!(!ledger::is_quiet());
    let r = std::panic::catch_unwind(|| ledger::quietly(|| panic!("a test's own failure")));
    assert!(r.is_err());
    assert!(!ledger::is_quiet());
    ledger::quietly(|| assert!(ledger::is_quiet()));
}

/// A known entry hides exactly what it names: a json_schema `text.format`
/// without its name is a violation, llama.cpp's bare one is known; a tool
/// missing its `name` is a violation, one missing only `strict` is known.
#[test]
fn a_known_entry_hides_only_what_it_names() {
    let url = "https://api.openai.test/v1/responses";
    let with_format = |f: Value| json!({"model": "m", "input": "hi", "text": {"format": f}});
    let no_name = check(
        Protocol::Openai,
        url,
        &with_format(json!({"type": "json_schema", "schema": {"type": "object"}})),
        &[],
    );
    assert!(
        no_name.iter().any(|l| l.contains("\"name\" is a required")),
        "{no_name:#?}"
    );
    let bare = check(
        Protocol::Openai,
        url,
        &with_format(json!({"type": "json_schema"})),
        &[],
    );
    assert!(bare.is_empty(), "{bare:#?}");
    // A json_object with an unknown key besides `schema` is not llama's spelling.
    let odd = check(
        Protocol::Openai,
        url,
        &with_format(json!({"type": "json_object", "schema": {}, "surprise": 1})),
        &[],
    );
    assert_eq!(odd.len(), 1, "{odd:#?}");

    let with_tool = |t: Value| json!({"model": "m", "input": "hi", "tools": [t]});
    let tool = |t: Value| check(Protocol::Openai, url, &with_tool(t), &[]);
    assert!(tool(json!({"type": "function", "name": "f", "parameters": {}})).is_empty());
    let nameless = tool(json!({"type": "function", "parameters": {}}));
    assert!(
        nameless
            .iter()
            .any(|l| l.contains("\"name\" is a required")),
        "{nameless:#?}"
    );
}

#[test]
fn a_known_violation_pattern_matches_by_segment() {
    assert!(known::glob("", ""));
    assert!(known::glob("/messages/*/content", "/messages/3/content"));
    assert!(!known::glob("/messages/*/content", "/messages/3/content/0"));
    assert!(known::glob(
        "/messages/**/input",
        "/messages/1/content/0/input"
    ));
    assert!(known::glob("/**", "/a/b"));
    assert!(!known::glob("/tools", "/messages"));
}

/// Every known violation still names a real entry point and says why.
#[test]
fn every_known_violation_says_where_and_why() {
    for k in known::KNOWN {
        assert!(
            k.entry.starts_with("OpenAI ")
                || k.entry.starts_with("Anthropic ")
                || k.entry.starts_with("Gemini "),
            "{}",
            k.entry
        );
        assert!(!k.reason.is_empty() && !k.keyword.is_empty());
        assert!(k.path.is_empty() || k.path.starts_with('/'), "{}", k.path);
    }
}

/// The pinned schemas say where they came from (scripts/api-schemas-update.sh).
#[test]
fn the_pinned_schemas_record_their_sources() {
    let sources: Value =
        serde_json::from_str(include_str!("../fixtures/api_schemas/SOURCES.json")).unwrap();
    for provider in ["openai", "anthropic", "gemini"] {
        let s = &sources[provider];
        for key in ["source", "fetched", "sha256", "sha256_canonical"] {
            assert!(s[key].is_string(), "{provider}.{key}: {s}");
        }
    }
    // The Anthropic derivation says what it ran on.
    let a = &sources["anthropic"];
    assert!(
        a["container_image"]
            .as_str()
            .is_some_and(|i| i.contains("@sha256:")),
        "{a}"
    );
    assert!(
        a["pip_freeze"].as_array().is_some_and(|l| l
            .iter()
            .any(|p| p.as_str().is_some_and(|p| p.starts_with("pydantic==")))),
        "{a}"
    );
}

/// A violation recorded anywhere in the process fails the process at its
/// exit: this test runs the probe below in a child process of the same
/// binary and reads its exit status.
#[test]
fn a_violation_fails_the_test_process() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .env("LMGW_API_COMPAT_PROBE", "1")
        .args([
            "--exact",
            "api_compat::violation_probe",
            "--ignored",
            "--test-threads=1",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 passed"),
        "the probe itself passes:\n{stdout}\n{stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "the process fails at its exit:\n{stdout}\n{stderr}"
    );
    assert!(stderr.contains("api-compat: 1 violation(s)"), "{stderr}");
}

/// Records one violation and passes; only [`a_violation_fails_the_test_process`]
/// runs it.
#[test]
#[ignore = "a probe: run by a_violation_fails_the_test_process in a child process"]
fn violation_probe() {
    // `--run-ignored all` runs this probe in the parent run too, where a
    // recorded violation would fail that run: only the child sets this.
    if std::env::var_os("LMGW_API_COMPAT_PROBE").is_none() {
        return;
    }
    let body = serde_json::to_vec(&anthropic_turns(json!(""))).unwrap();
    egress::watched(&Built {
        protocol: Protocol::Anthropic,
        url: "https://api.anthropic.test/v1/messages",
        body: &body,
        passthrough: &serde_json::Map::new(),
    });
}
