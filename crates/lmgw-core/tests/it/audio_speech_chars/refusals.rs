//! What a refusal of nothing-to-say names and logs (review TC-13..TC-19):
//! the alias the client asked for, the full codepoint list in the log, a
//! text the engine's own rewrites blank. (The check on the row that
//! answers is reachable by no request a test can make: a fallback is never
//! a local row, and a cloud one has no vocabulary.)

use serde_json::{json, Value};

use super::{answer, code, create, each, gateway, header, session, spoken, supertonic};
use crate::common::captured_log::capture_log;
use crate::support::audio_world::world;
use crate::support::audiocpp_gguf;

fn message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_default()
}

/// 70 distinct emoji: more than the header names (`+N more`).
fn many() -> (u32, u32) {
    (0x1F600, 0x1F600 + 69)
}

#[tokio::test]
async fn speech_refused_before_admission_names_the_alias_and_logs_every_character() {
    let (log, _guard) = capture_log();
    let w = world().await;
    w.row("st", "supertonic", audiocpp_gguf::supertonic).await;
    w.answer_wav().await;

    let (first, last) = many();
    let input: String = (first..=last).filter_map(char::from_u32).collect();
    let resp = w.speak(json!({"model": "audio/st", "input": input})).await;
    assert_eq!(resp.status(), 400);
    // The header is bounded; the log line is not.
    assert!(
        header(&resp).unwrap().contains("more"),
        "{:?}",
        header(&resp)
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(code(&body), "empty_input", "{body}");
    assert!(message(&body).contains("'audio/st'"), "{body}");
    let text = log.text();
    assert!(
        text.contains("speech: TTS 'audio/st': characters its engine cannot say: dropped U+1F600")
            && text.contains(&format!("U+{last:X}")),
        "{text}"
    );
    assert_eq!(w.runs(), 0);
}

/// A text left blank only by the engine's own rewrites (`♥` is written as
/// nothing, `#` as a space) would be said as a "." blip: `empty_input`.
#[tokio::test]
async fn a_text_the_engine_s_own_rewrites_blank_is_refused_on_speech() {
    let w = world().await;
    w.row("st", "supertonic", audiocpp_gguf::supertonic).await;
    w.answer_wav().await;
    for input in [
        "\u{2665}\u{1F60A}",
        "#\u{FE0F}\u{20E3}",
        "\u{2192} \u{1F60A}",
    ] {
        let resp = w.speak(json!({"model": "audio/st", "input": input})).await;
        assert_eq!(resp.status(), 400, "{input:?}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(code(&body), "empty_input", "{body}");
    }
    // Nothing fitted, and still nothing to say (TC-20): a lone `♥`, white
    // space; and an empty input (TC-21) on any family's row.
    for input in ["\u{2665}", " ", "\n\u{3000}", ""] {
        let resp = w.speak(json!({"model": "audio/st", "input": input})).await;
        assert_eq!(resp.status(), 400, "{input:?}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(code(&body), "empty_input", "{body}");
    }
    assert_eq!(w.runs(), 0, "no container was started");
    // Said text with the same characters goes.
    let resp = w
        .speak(json!({"model": "audio/st", "input": "Danke \u{2665}\u{1F60A}"}))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(w.sent().await.len(), 1);
}

#[tokio::test]
async fn a_task_refused_before_admission_names_the_alias_and_logs_every_character() {
    let (log, _guard) = capture_log();
    let w = world().await;
    w.row("st", "supertonic", audiocpp_gguf::supertonic).await;
    let resp =
        w.gw.client()
            .post(format!("{}/v1/tasks/run", w.gw))
            .json(&json!({"model": "audio/st", "request": {"text": "\u{1F60A}\u{1F389}"}}))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(code(&body), "empty_input", "{body}");
    assert!(message(&body).contains("'audio/st'"), "{body}");
    let text = log.text();
    assert!(
        text.contains("task: 'audio/st': characters its engine cannot say: dropped U+1F60A")
            && text.contains("U+1F389"),
        "{text}"
    );
    assert_eq!(w.runs(), 0);
}

/// TC-21: every engine turns an empty text down, after a container start and
/// as a 502, so a row without a vocabulary is refused before one too.
#[tokio::test]
async fn an_empty_input_is_empty_input_on_a_row_without_a_vocabulary() {
    let w = world().await;
    w.row("kk", "kokoro_tts", |_| {}).await;
    w.answer_wav().await;
    for input in ["", " ", "\n"] {
        let resp = w.speak(json!({"model": "audio/kk", "input": input})).await;
        assert_eq!(resp.status(), 400, "{input:?}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(code(&body), "empty_input", "{body}");
    }
    assert_eq!(w.runs(), 0, "no container was started");
}

/// TC-22: the realtime turn is the path the series exists for. A clause the
/// engine's rewrites blank (`♥😊`, fitted to `♥`; a lone `♥`) is skipped and
/// the answer completes with exactly the one request for the rest.
#[tokio::test]
async fn a_clause_the_engine_s_rewrites_blank_is_skipped_in_a_realtime_answer() {
    for tail in ["\u{2665}\u{1F60A}", "\u{2665}"] {
        let (g, addr, chat) = gateway(&[supertonic()], 1, |_| {}).await;
        let (mut ws, _) = session(&addr, json!({})).await;
        let n = spoken(&g);
        let events = answer(&mut ws, &chat, &["Danke! ", tail], create()).await;
        let said: Vec<Value> = each(&g, n, "input");
        assert_eq!(said, [json!("Danke!")], "{tail:?}: one request");
        assert!(g.world().refused_speech.is_empty(), "{tail:?}: none empty");
        let done = &events.last().unwrap()["response"];
        assert_eq!(done["status"], "completed", "{tail:?}");
    }
}

/// TC-22: the task route refuses the same texts, and an empty one, before
/// anything starts.
#[tokio::test]
async fn a_task_text_the_engine_s_rewrites_blank_is_refused() {
    let w = world().await;
    w.row("st", "supertonic", audiocpp_gguf::supertonic).await;
    for text in ["\u{2665}\u{1F60A}", "\u{2665}", "", " "] {
        let resp =
            w.gw.client()
                .post(format!("{}/v1/tasks/run", w.gw))
                .json(&json!({"model": "audio/st", "request": {"text": text}}))
                .send()
                .await
                .unwrap();
        assert_eq!(resp.status(), 400, "{text:?}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(code(&body), "empty_input", "{text:?}: {body}");
    }
    assert_eq!(w.runs(), 0, "no container was started");
}
