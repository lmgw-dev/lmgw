//! A heard response's attempt and retry (voice-audio-input design §3.5,
//! §7): which refusals go again as the transcript, what the relay sees of
//! a refused attempt, the bound responder end to end against a cloud mock —
//! the audio refused before a byte leaves, the retry over the row once the
//! journal settled it, the skip, a failed transcription, a veto. The
//! skipped attempt's ends are `skip`, the session's refusal memory
//! `memory`.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::watch;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{Protocol, UpstreamKind};
use crate::ir::{Completion, ContentPart};
use crate::proxy::stop_pair;
use crate::state::AppState;
use crate::store::{self, NewAlias, NewUpstream};
use crate::web::chat_voice::bound::UserRow;

mod memory;
mod screen;
mod skip;

fn audio() -> ContentPart {
    ContentPart::Audio {
        mime: "audio/wav".into(),
        data: "UklGRg==".into(),
    }
}

fn upstream(status: u16, message: &str) -> GatewayError {
    GatewayError::Upstream {
        status,
        provider_type: None,
        message: message.into(),
    }
}

#[test]
fn only_refusals_of_the_audio_go_again_as_the_transcript() {
    let unheard = crate::web::chat_voice::bound::AUDIO_NOT_HEARD;
    let retried = [
        (
            GatewayError::InvalidRequest {
                code: unheard,
                message: "x".into(),
            },
            Refusal::Unheard,
        ),
        (
            GatewayError::Unsupported(crate::egress::anthropic::NO_AUDIO_BLOCK.into()),
            Refusal::Unheard,
        ),
        (
            GatewayError::Unsupported(crate::gate::count::AUDIO_UNBOUNDED.into()),
            Refusal::Guard,
        ),
        (
            GatewayError::ContextExceeded {
                model: "gemma".into(),
                prompt_tokens: 5000,
                max_output: None,
                limit: 4096,
                top_rung: None,
            },
            Refusal::Server,
        ),
        (upstream(400, "invalid audio"), Refusal::Server),
        (upstream(413, "too large"), Refusal::Server),
        (
            upstream(
                500,
                "audio input is not supported - hint: if this is unexpected, you may need to \
                 provide the mmproj",
            ),
            Refusal::Server,
        ),
        (
            upstream(500, "Failed to load image or audio file"),
            Refusal::Server,
        ),
    ];
    for (e, kind) in retried {
        assert_eq!(refusal(&e, false), Some(kind), "{e:?}");
    }
    // A media part the server could not load is the audio only when the
    // request carried no image (WP2 review #3): a broken image fails alike.
    let unloadable = upstream(500, "Failed to load image or audio file");
    assert_eq!(refusal(&unloadable, true), None);
    let unsupported = upstream(500, "audio input is not supported");
    assert_eq!(refusal(&unsupported, true), Some(Refusal::Server));
    let not_retried = [
        upstream(500, "CUDA error: out of memory"),
        upstream(502, "bad gateway"),
        upstream(503, "Loading model"),
        upstream(429, "slow down"),
        GatewayError::Transport("connection reset".into()),
        GatewayError::Timeout,
        GatewayError::GpuHold {
            model: "gemma".into(),
            detail: String::new(),
        },
        GatewayError::GpuBenchmark {
            model: "gemma".into(),
            run_id: 1,
            detail: String::new(),
        },
        GatewayError::VramQueueTimeout {
            model: "gemma".into(),
            waited_seconds: 1,
            holding: String::new(),
        },
        GatewayError::VramTooLarge {
            model: "gemma".into(),
            need: "1".into(),
            headroom: "0".into(),
            capacity: "1".into(),
        },
        GatewayError::Unsupported("another facet".into()),
        GatewayError::InvalidRequest {
            code: "superseded",
            message: "x".into(),
        },
        crate::proxy::canceled("stopped"),
        GatewayError::KeyScope {
            key: "k".into(),
            alias: "a".into(),
            reason: "no".into(),
        },
    ];
    for e in not_retried {
        assert_eq!(refusal(&e, false), None, "{e:?}");
    }
    // A llama-server going away under the audio is no refusal: kept, not
    // retried. Anywhere else a dropped connection says nothing about the
    // audio (decision D5).
    let reset = GatewayError::Transport("connection reset".into());
    assert!(crashed(&reset, true));
    assert!(!crashed(&reset, false), "a cloud route's drop");
    // …which goes again as the transcript, once (review V4).
    assert!(dropped(&reset, false));
    assert!(!dropped(&reset, true), "a llama-server's drop is a crash");
    assert!(!dropped(&upstream(502, "bad gateway"), false));
    assert!(!dropped(&GatewayError::Timeout, false));
    assert!(crashed(
        &upstream(
            502,
            "chat model 'gemma' stopped answering (reset) and could not be restarted: oom"
        ),
        true
    ));
    assert!(!crashed(&upstream(502, "bad gateway"), true));
    assert!(!crashed(&upstream(500, "CUDA error: out of memory"), true));
}

fn frame(event: &'static str) -> TurnFrame {
    TurnFrame::new(event, "{}".into())
}

fn events_of(frames: &[TurnFrame]) -> Vec<&str> {
    frames.iter().map(|f| f.event).collect()
}

/// An attempt with the audio, armed, for the thread's model `gemma`.
fn armed(tx: &responder::Tx) -> Attempt {
    let mut a = Attempt::new(None, 1, tx, None);
    a.armed = true;
    a.model = "gemma".into();
    a
}

#[test]
fn a_refused_attempt_says_nothing_and_a_refusal_after_its_first_word_is_relayed() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let refused = GatewayError::InvalidRequest {
        code: crate::web::chat_voice::bound::AUDIO_NOT_HEARD,
        message: "gpt does not take audio input".into(),
    };
    // Before any output: the refusal and the `done` behind it are held.
    let mut a = armed(&tx);
    assert_eq!(
        events_of(&a.screen(frame("state"))),
        ["state"],
        "a model state goes on"
    );
    assert!(a.screen(TurnFrame::error(&refused)).is_empty());
    assert!(a.screen(frame("done")).is_empty());
    assert_eq!(a.refused.as_ref().map(|r| r.kind), Some(Refusal::Unheard));
    // After a delta it is the turn's own error.
    let mut a = armed(&tx);
    assert_eq!(a.screen(frame("delta")).len(), 1);
    assert_eq!(a.screen(TurnFrame::error(&refused)).len(), 1);
    assert!(a.refused.is_none());
    // A refusal the transcript would meet as well is relayed.
    let mut a = armed(&tx);
    let hold = GatewayError::GpuHold {
        model: "gemma".into(),
        detail: String::new(),
    };
    assert_eq!(a.screen(TurnFrame::error(&hold)).len(), 1);
    assert_eq!(a.screen(frame("done")).len(), 1);
    // A transcript turn is never screened.
    let mut a = Attempt::new(None, 1, &tx, None);
    assert_eq!(a.screen(TurnFrame::error(&refused)).len(), 1);
}

// -- the bound responder, end to end ----------------------------------------

/// A gateway whose alias `m` is a cloud model answering `Es ist spät.`
/// whose catalog says nothing of audio (so lmgw cannot tell whether it
/// takes it), with the audio-input setting at `setting`, and a thread on it
/// whose history ends with a reply: the thread's id.
async fn world(setting: &str) -> (crate::state::SharedState, MockServer, i64) {
    let mock = MockServer::start().await;
    let said = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": "Es ist spät."}}]})
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(said, "text/event-stream"))
        .mount(&mock)
        .await;
    let state = AppState::init_for_tests().await.unwrap();
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "cloud".into(),
            protocol: Protocol::Openai,
            kind: UpstreamKind::Generic,
            base_url: mock.uri(),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "m".into(),
            upstream_id: up,
            upstream_model_id: "gpt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
        },
    )
    .await
    .unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.chat_voice_audio_input = setting.into();
    store::save_settings(&state.db, &settings).await.unwrap();
    state.reload_snapshot().await.unwrap();
    let tid = store::create_chat_thread(&state.db, "m", "chat")
        .await
        .unwrap();
    for (role, text) in [("user", "hi"), ("assistant", "Hallo!")] {
        store::append_chat_message(&state.db, tid, role, text, "", None, None, None)
            .await
            .unwrap();
    }
    (state, mock, tid)
}

/// What a bound response sent the core.
#[derive(Default, Debug)]
struct Said {
    frames: Vec<(&'static str, Value)>,
    notes: Vec<String>,
    /// What the session keeps: the model and its `why`.
    remembered: Vec<(String, String)>,
    result: Option<Result<Completion, GatewayError>>,
}

/// Run a heard response of thread `tid` (text output) whose new turn is
/// `spoken`, the journal settling its row with `settle` once the test
/// has seen it wait.
async fn respond(
    state: &crate::state::SharedState,
    tid: i64,
    spoken: Vec<ContentPart>,
    settle: impl FnOnce() -> futures::future::BoxFuture<'static, UserRow> + Send + 'static,
) -> Said {
    respond_until(state, tid, spoken, Ending::Row(Box::new(settle))).await
}

/// How a test ends a heard response's wait for its row.
enum Ending {
    /// The journal says this.
    Row(Box<dyn FnOnce() -> futures::future::BoxFuture<'static, UserRow> + Send>),
    /// The response's stop is raised.
    Stop,
}

/// [`respond`], ended by `ending`.
async fn respond_until(
    state: &crate::state::SharedState,
    tid: i64,
    spoken: Vec<ContentPart>,
    ending: Ending,
) -> Said {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (stop, signal) = stop_pair();
    let (user_tx, user) = tokio::sync::oneshot::channel();
    // An audio response's user entry answers no id (§3.3).
    user_tx.send(Ok(None)).unwrap();
    let (row_tx, user_row) = watch::channel(None);
    let job = super::super::Job {
        state: state.clone(),
        ctx: Default::default(),
        gen: 1,
        label: "realtime test".into(),
        thread_id: tid,
        tx,
        stop: signal,
        user,
        journal: None,
        speaking: None,
        hint: false,
        audio: Some(Launch {
            parts: spoken.into_iter().map(Spoken::Ready).collect(),
            user_row,
        }),
        degraded: None,
    };
    let running = tokio::spawn(super::super::run(job));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !running.is_finished(),
        "the response waits for its user row"
    );
    match ending {
        Ending::Row(settle) => {
            row_tx.send_replace(Some(settle().await));
        }
        Ending::Stop => stop.stop(),
    }
    tokio::time::timeout(Duration::from_secs(20), running)
        .await
        .expect("the response ends")
        .unwrap();
    let mut said = Said::default();
    while let Ok((_, m)) = rx.try_recv() {
        match m {
            Msg::ChatFrame { event, data } => said.frames.push((event, data)),
            Msg::Input { input, why } => {
                assert_eq!(input, InputPath::Transcript);
                said.notes.push(why);
            }
            Msg::Refused { refused, .. } => said.remembered.push((refused.model, refused.why)),
            Msg::Finished(r) => said.result = Some(*r),
            _ => {}
        }
    }
    said
}

/// The chat bodies the mock got.
async fn bodies(mock: &MockServer) -> Vec<Value> {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/chat/completions")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// The journal writing the turn's row, as it does once it is transcribed.
fn writes_row(
    state: &crate::state::SharedState,
    tid: i64,
    text: &'static str,
) -> impl FnOnce() -> futures::future::BoxFuture<'static, UserRow> {
    let state = state.clone();
    move || {
        Box::pin(async move {
            let id = store::append_chat_message(&state.db, tid, "user", text, "", None, None, None)
                .await
                .unwrap();
            UserRow::Written(id)
        })
    }
}

fn settles(row: UserRow) -> impl FnOnce() -> futures::future::BoxFuture<'static, UserRow> {
    move || Box::pin(async move { row })
}

fn events(said: &Said) -> Vec<&str> {
    said.frames.iter().map(|(e, _)| *e).collect()
}

#[tokio::test]
async fn audio_refused_before_it_leaves_goes_again_as_the_row_with_no_turn_frame() {
    let (state, mock, tid) = world("on").await;
    let said = respond(
        &state,
        tid,
        vec![audio()],
        writes_row(&state, tid, "Wie spät ist es?"),
    )
    .await;
    let reply = said.result.as_ref().unwrap().as_ref().expect("answered");
    assert_eq!(reply.content, vec![ContentPart::text("Es ist spät.")]);
    let ev = events(&said);
    assert!(!ev.contains(&"turn"), "no turn frame: {ev:?}");
    assert!(!ev.contains(&"error"), "the refusal is not relayed: {ev:?}");
    assert_eq!(ev.iter().filter(|e| **e == "done").count(), 1, "{ev:?}");
    assert_eq!(said.notes.len(), 1, "{:?}", said.notes);
    let why = &said.notes[0];
    assert_eq!(
        why,
        "lmgw cannot tell whether m takes audio (upstream 'cloud'), so nothing was sent"
    );
    assert!(
        said.remembered.is_empty(),
        "lmgw's own refusal is never remembered"
    );
    // The cloud got the transcript, once, and never the audio.
    let got = bodies(&mock).await;
    assert_eq!(got.len(), 1, "{got:?}");
    let text = got[0].to_string();
    assert!(
        !text.contains("input_audio") && !text.contains("UklGRg"),
        "{text}"
    );
    assert_eq!(
        got[0]["messages"].as_array().unwrap().last().unwrap()["content"],
        "Wie spät ist es?"
    );
    // The reply follows the row.
    let rows = store::list_chat_messages(&state.db, tid).await.unwrap();
    let tail: Vec<_> = rows
        .iter()
        .rev()
        .take(2)
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(tail, ["Es ist spät.", "Wie spät ist es?"]);
}

#[tokio::test]
async fn a_thread_whose_setting_is_off_by_now_skips_the_audio_and_waits_for_the_row() {
    let (state, mock, tid) = world("off").await;
    let said = respond(
        &state,
        tid,
        vec![audio()],
        writes_row(&state, tid, "Wie spät ist es?"),
    )
    .await;
    assert!(said.result.as_ref().unwrap().is_ok(), "{said:?}");
    assert_eq!(said.notes, ["audio input is off (Settings → Chat → Voice)"]);
    assert!(said.remembered.is_empty());
    let got = bodies(&mock).await;
    assert_eq!(got.len(), 1);
    assert!(!got[0].to_string().contains("input_audio"));
    assert!(!events(&said).contains(&"turn"));
}

#[tokio::test]
async fn a_refusal_and_a_failed_transcription_fail_with_transcription_failed() {
    let (state, mock, tid) = world("on").await;
    let said = respond(&state, tid, vec![audio()], settles(UserRow::Failed)).await;
    let e = said.result.unwrap().unwrap_err();
    assert_eq!(e.code(), "transcription_failed", "{e}");
    assert_eq!(e.http_status().as_u16(), 500);
    let codes: Vec<_> = said
        .frames
        .iter()
        .filter(|(e, _)| *e == "error")
        .map(|(_, d)| d["code"].clone())
        .collect();
    assert_eq!(codes, [json!("transcription_failed")], "{:?}", said.frames);
    let message = said.frames.iter().find(|(e, _)| *e == "error").unwrap().1["message"].clone();
    assert!(
        !message.as_str().unwrap().contains("transcription.failed"),
        "the audio path sends no transcription.failed to point at: {message}"
    );
    assert!(bodies(&mock).await.is_empty(), "nothing was sent");
}

#[tokio::test]
async fn a_veto_while_the_refused_attempt_waits_ends_quietly() {
    let (state, mock, tid) = world("on").await;
    let said = respond(&state, tid, vec![audio()], settles(UserRow::Veto)).await;
    assert_eq!(events(&said), ["done"], "no error: {:?}", said.frames);
    let e = said.result.unwrap().unwrap_err();
    assert!(crate::proxy::is_canceled(&e), "{e}");
    assert!(bodies(&mock).await.is_empty());
    let rows = store::list_chat_messages(&state.db, tid).await.unwrap();
    assert_eq!(rows.len(), 2, "nothing written");
}
