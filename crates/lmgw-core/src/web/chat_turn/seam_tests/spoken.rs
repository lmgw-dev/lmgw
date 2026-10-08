//! Voice turns the model hears, at the turn seam (voice-audio-input design
//! §3.4, §7): what a turn sends when its caller hands it spoken parts, and
//! that a turn handed none sends today's bytes.
//!
//! **The `off` golden.** With no spoken parts — every turn while the setting
//! is `off`, and every page turn — the upstream gets the request it got
//! before the feature: the bodies are compared with
//! `tests/fixtures/chat_requests/<name>.json`, captured before the request
//! learned spoken parts. `LMGW_BLESS=1` rewrites them (the diff then shows
//! what changed); without it, a missing file fails.

use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::store::{MessageVoice, VIA_REALTIME};

/// An OpenAI-shaped SSE answer saying `text`.
fn said(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": text}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    )
}

/// One streamed call of tool `name` with no arguments.
fn calls(name: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function",
            "function": {"name": name, "arguments": "{}"}}]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    )
}

/// Mount `bodies` as the answers to successive chat calls.
async fn answers(mock: &MockServer, bodies: &[String]) {
    for (i, body) in bodies.iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(body.clone(), "text/event-stream"),
            )
            .up_to_n_times(1)
            .with_priority((i + 1) as u8)
            .mount(mock)
            .await;
    }
}

/// The chat bodies the upstream got, in order (not its catalog reads).
async fn bodies(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/chat/completions")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// A spoken user row, as a bound session's journal writes one.
fn spoken_row() -> MessageVoice {
    MessageVoice {
        via: VIA_REALTIME.into(),
        asr: Some("parakeet".into()),
        audio_ms: Some(2_200),
        ..Default::default()
    }
}

/// [`world`]'s thread with a spoken history after its `hi`: a reply, then
/// a spoken turn answered, then a spoken turn still owed — the thread a
/// bound session's next response meets. The thread has a prompt.
async fn spoken_world(base: &str, kind: &str) -> (SharedState, ChatThread) {
    let (state, mut thread, _) = world(base, kind).await;
    let repo = ChatRepo::of(thread.id);
    let reply = |text: &'static str| {
        let state = state.clone();
        async move {
            store::append_chat_message(
                &state.db,
                thread.id,
                "assistant",
                text,
                "",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }
    };
    reply("Hallo!").await;
    repo.append_user_message(
        &state,
        thread.id,
        "Wie spät ist es?",
        &[],
        &[],
        Some(&spoken_row()),
        &crate::web::chat_caller::Caller::default(),
    )
    .await
    .unwrap();
    reply("Das weiß ich leider nicht.").await;
    repo.append_user_message(
        &state,
        thread.id,
        "Und morgen?",
        &[],
        &[],
        Some(&spoken_row()),
        &crate::web::chat_caller::Caller::default(),
    )
    .await
    .unwrap();
    thread.system_prompt = "Be brief.".into();
    (state, thread)
}

/// A bound session's turn options as a speaking response passes them, with
/// nothing spoken: the voice block and the language.
fn voice_opts() -> TurnOpts {
    TurnOpts {
        voice: Some(VoiceTurn { hint: None }),
        language: Some(TurnLanguage {
            reply: "de".into(),
            speaks: Some("de".into()),
            spoken: true,
        }),
        ..Default::default()
    }
}

/// Compare `got` with the golden request bodies `name`.
fn golden(name: &str, got: &[Value]) {
    let got = serde_json::to_string_pretty(got).unwrap() + "\n";
    let file = format!(
        "{}/tests/fixtures/chat_requests/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    if std::env::var_os("LMGW_BLESS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(&file).parent().unwrap()).unwrap();
        std::fs::write(&file, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&file)
        .unwrap_or_else(|e| panic!("{file}: {e} — run the test with LMGW_BLESS=1 to capture it"));
    assert!(
        got == want,
        "the requests '{name}' changed\n--- want ({file})\n{want}\n--- got\n{got}"
    );
}

/// A turn handed no spoken parts sends today's bytes: a page's turn, a
/// bound session's voice turn, and a tool thread's two model calls (the
/// tools list left out: it is the self-admin catalog's, which changes on
/// its own).
#[tokio::test]
async fn without_spoken_parts_the_requests_are_todays() {
    let mut got = Vec::new();
    for opts in [TurnOpts::default(), voice_opts()] {
        let mock = MockServer::start().await;
        answers(&mock, &[said("ok")]).await;
        let (state, thread) = spoken_world(&mock.uri(), "chat").await;
        let mut rx = start(&state, &thread, 0, opts).await;
        rest(&mut rx).await;
        got.extend(bodies(&mock).await);
    }
    golden("page_and_voice", &got);

    let mock = MockServer::start().await;
    answers(&mock, &[calls("lmgw__mcp_servers"), said("keine")]).await;
    let (state, thread) = spoken_world(&mock.uri(), "admin").await;
    let mut rx = start(&state, &thread, 0, voice_opts()).await;
    let frames = rest(&mut rx).await;
    assert_eq!(frames.last().unwrap().1["saved"], true, "{frames:?}");
    let mut got = bodies(&mock).await;
    assert_eq!(got.len(), 2, "a tool call and the answer: {got:?}");
    for b in &mut got {
        assert!(b["tools"].as_array().is_some_and(|t| !t.is_empty()), "{b}");
        b.as_object_mut().unwrap().remove("tools");
    }
    golden("voice_tools", &got);
}

// ---------------------------------------------------------------------------
// Capability: audio goes only to a model that takes it, wherever it runs
// ---------------------------------------------------------------------------

fn audio() -> crate::ir::ContentPart {
    crate::ir::ContentPart::Audio {
        mime: "audio/wav".into(),
        data: "UklGRgAAAABXQVZF".into(),
    }
}

/// Turn options carrying the user's speech as audio, the row settled.
fn heard() -> (TurnOpts, tokio::sync::watch::Sender<Option<UserRow>>) {
    let (row, user_row) = tokio::sync::watch::channel(Some(UserRow::NoRow));
    let opts = TurnOpts {
        spoken: Some(vec![audio()]),
        user_row: Some(user_row),
        ..voice_opts()
    };
    (opts, row)
}

/// A heard turn's frames, to its end.
async fn heard_frames(state: &SharedState, thread: &ChatThread) -> Vec<(String, Value)> {
    let (opts, _row) = heard();
    let (tx, mut rx) = mpsc::channel(64);
    let mode = TurnMode::Fresh {
        user_message_id: None,
    };
    start_turn_into(
        state,
        ChatRepo::of(thread.id),
        thread,
        mode,
        Caps::default(),
        tx,
        opts,
    )
    .await
    .unwrap();
    rest(&mut rx).await
}

/// The turn's frames, its `error` code, and that nothing reached `mock` but
/// the capability lookup's catalog read — no chat body, and on the tool
/// path not the reasoning fit's catalog read either (WP2 review #8: its
/// refusal comes before the fit).
async fn refused_unsent(
    state: &SharedState,
    thread: &ChatThread,
    mock: &MockServer,
) -> Vec<(String, Value)> {
    let frames = heard_frames(state, thread).await;
    let error = frames.iter().find(|(e, _)| e == "error").expect("an error");
    assert_eq!(error.1["code"], AUDIO_NOT_HEARD, "{frames:?}");
    assert_eq!(
        frames.last().unwrap(),
        &("done".into(), json!({"aborted": true}))
    );
    assert!(bodies(mock).await.is_empty(), "nothing was sent");
    let reached: Vec<String> = mock
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert!(
        reached.iter().all(|r| r == "GET /models"),
        "only the capability lookup reached it: {reached:?}"
    );
    assert!(!frames.iter().any(|(e, _)| e == "turn"), "{frames:?}");
    frames
}

/// Say that alias `m` (the seam world's cloud model) has the capabilities
/// `caps`; `None`: nothing, so lmgw cannot tell.
async fn m_says(state: &SharedState, caps: Option<Value>) {
    sqlx::query("UPDATE models SET capabilities_override = ?1 WHERE alias = 'm'")
        .bind(caps.map(|c| json!({ "capabilities": c }).to_string()))
        .execute(&state.db)
        .await
        .unwrap();
    state.reload_snapshot().await.unwrap();
}

/// A local model `gemma` that takes audio, under the GPU hold, whose
/// fallback is `m`.
async fn held_gemma(state: &SharedState, thread: &mut ChatThread) {
    store::insert_local_model(
        &state.db,
        &store::NewLocalModel {
            model_id: "gemma".into(),
            gguf_path: "gemma.gguf".into(),
            params: Default::default(),
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            capabilities_override: Some(json!({ "capabilities": {
                "task": "chat", "input_modalities": ["text", "audio"]
            } })),
            ladder: vec![],
        },
    )
    .await
    .unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.hold.active = true;
    settings.hold.fallback_alias = Some("m".into());
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    thread.model_alias = "gemma".into();
}

/// The request rows the turn wrote: `(ingress_proto, status, error_kind)`.
async fn request_rows(state: &SharedState) -> Vec<(String, i64, Option<String>)> {
    use sqlx::Row;
    sqlx::query("SELECT ingress_proto, status, error_kind FROM request_logs ORDER BY id")
        .fetch_all(&state.db)
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

/// The one row a refusal of the audio writes, on either path (WP2 review
/// #8: the tool path wrote none).
async fn one_refusal_row(state: &SharedState, kind: &str) {
    let proto = if kind == "admin" { "admin" } else { "chat" };
    assert_eq!(
        request_rows(state).await,
        [(proto.to_string(), 400, Some(AUDIO_NOT_HEARD.to_string()))],
        "{kind}"
    );
}

/// A model lmgw cannot judge never gets a turn's audio (decision D1): the
/// plain stream and the tool loop refuse its route before a byte leaves,
/// and save nothing. A model that reads text only alike.
#[tokio::test]
async fn a_route_whose_model_cannot_hear_is_refused_the_audio_before_anything_is_sent() {
    for (kind, caps) in [
        ("chat", None),
        ("admin", None),
        (
            "chat",
            Some(json!({ "task": "chat", "input_modalities": ["text"] })),
        ),
    ] {
        let mock = MockServer::start().await;
        answers(&mock, &[said("never")]).await;
        let (state, thread) = spoken_world(&mock.uri(), kind).await;
        m_says(&state, caps).await;
        refused_unsent(&state, &thread, &mock).await;
        one_refusal_row(&state, kind).await;
        let rows = store::list_chat_messages(&state.db, thread.id)
            .await
            .unwrap();
        assert_eq!(
            rows.last().unwrap().content,
            "Und morgen?",
            "{kind}: nothing saved"
        );
    }
}

/// A local model the GPU hold hands to a cloud fallback that takes audio
/// (changed 2026-10-06: the configured fallback is always used, and hears
/// the turn when it can) — the plain stream and the tool loop alike: the
/// fallback gets the turn's audio, and the reply is saved after the row.
#[tokio::test]
async fn the_holds_cloud_fallback_that_hears_gets_the_audio() {
    for kind in ["chat", "admin"] {
        let mock = MockServer::start().await;
        answers(&mock, &[said("Morgen wird es sonnig.")]).await;
        let (state, mut thread) = spoken_world(&mock.uri(), kind).await;
        m_says(
            &state,
            Some(json!({ "task": "chat", "input_modalities": ["text", "audio"] })),
        )
        .await;
        held_gemma(&state, &mut thread).await;
        let frames = heard_frames(&state, &thread).await;
        assert!(
            !frames.iter().any(|(e, _)| e == "error"),
            "{kind}: {frames:?}"
        );
        let got = bodies(&mock).await;
        assert_eq!(got.len(), 1, "{kind}: {got:?}");
        let last = got[0]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        let audio_parts = last["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["type"] == "input_audio")
            .count();
        assert_eq!(audio_parts, 1, "{kind}: {last}");
        let rows = store::list_chat_messages(&state.db, thread.id)
            .await
            .unwrap();
        assert_eq!(
            rows.last().unwrap().content,
            "Morgen wird es sonnig.",
            "{kind}: the reply is saved"
        );
    }
}

/// The same fallback, reading text only: the swap the gate makes after
/// the verdict is refused the audio on the route it settled on, named by
/// the fallback's upstream — the plain stream and the tool loop alike.
#[tokio::test]
async fn the_holds_fallback_that_reads_text_only_is_refused_the_audio() {
    for kind in ["chat", "admin"] {
        let mock = MockServer::start().await;
        answers(&mock, &[said("never")]).await;
        let (state, mut thread) = spoken_world(&mock.uri(), kind).await;
        m_says(
            &state,
            Some(json!({ "task": "chat", "input_modalities": ["text"] })),
        )
        .await;
        held_gemma(&state, &mut thread).await;
        let frames = refused_unsent(&state, &thread, &mock).await;
        let error = &frames.iter().find(|(e, _)| e == "error").unwrap().1;
        assert_eq!(
            error["message"],
            "m does not take audio input (upstream 'test-up'), so nothing was sent",
            "{kind}: {error}"
        );
        one_refusal_row(&state, kind).await;
    }
}

// ---------------------------------------------------------------------------
// The pre-save barrier
// ---------------------------------------------------------------------------

/// A heard response whose new turn is text (the cloud mock may take it):
/// its frames once the journal said `answer` — after the reply streamed —
/// and the thread's last two rows.
async fn saved_after(answer: Option<UserRow>) -> (Vec<(String, Value)>, Vec<String>) {
    let mock = MockServer::start().await;
    answers(&mock, &[said("Morgen wird es sonnig.")]).await;
    let (state, thread) = spoken_world(&mock.uri(), "chat").await;
    let (row, user_row) = tokio::sync::watch::channel(None);
    let opts = TurnOpts {
        spoken: Some(vec![crate::ir::ContentPart::text("Und übermorgen?")]),
        user_row: Some(user_row),
        ..voice_opts()
    };
    let (tx, mut rx) = mpsc::channel(64);
    let mode = TurnMode::Fresh {
        user_message_id: None,
    };
    start_turn_into(
        &state,
        ChatRepo::of(thread.id),
        &thread,
        mode,
        Caps::default(),
        tx,
        opts,
    )
    .await
    .unwrap();
    let head = until(&mut rx, "stop").await;
    // The reply streamed; it is not saved before the row is settled.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        rx.try_recv().is_err(),
        "it waits for its user row: {head:?}"
    );
    match answer {
        Some(UserRow::Written(_)) => {
            // As the journal writes it: an insert that moves no generation.
            let id = store::append_chat_message(
                &state.db,
                thread.id,
                "user",
                "Und übermorgen?",
                "",
                None,
                None,
                None,
            )
            .await
            .unwrap();
            row.send_replace(Some(UserRow::Written(id)));
        }
        Some(a) => {
            row.send_replace(Some(a));
        }
        None => drop(row),
    }
    let frames = rest(&mut rx).await;
    let rows = store::list_chat_messages(&state.db, thread.id)
        .await
        .unwrap();
    let tail = rows
        .iter()
        .rev()
        .take(2)
        .map(|m| m.content.clone())
        .collect();
    (frames, tail)
}

#[tokio::test]
async fn the_reply_is_saved_after_its_user_row_and_never_on_a_veto() {
    let (frames, tail) = saved_after(Some(UserRow::Written(0))).await;
    assert_eq!(frames.last().unwrap().1["saved"], true, "{frames:?}");
    assert_eq!(tail, ["Morgen wird es sonnig.", "Und übermorgen?"]);

    let (frames, tail) = saved_after(Some(UserRow::Veto)).await;
    let done = &frames.last().unwrap().1;
    assert_eq!(
        (done["saved"].clone(), done["message_id"].clone()),
        (json!(false), json!(0))
    );
    assert!(
        !frames.iter().any(|(e, _)| e == "error"),
        "a veto is quiet: {frames:?}"
    );
    assert_eq!(tail, ["Und morgen?", "Das weiß ich leider nicht."]);

    let (frames, tail) = saved_after(None).await;
    let error = &frames.iter().find(|(e, _)| e == "error").expect("said").1;
    assert_eq!(error["code"], "not_saved", "{frames:?}");
    assert_eq!(tail, ["Und morgen?", "Das weiß ich leider nicht."]);
}
