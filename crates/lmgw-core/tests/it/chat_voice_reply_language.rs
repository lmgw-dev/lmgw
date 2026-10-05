//! The reply language beside the spoken one (chat-voice design §2.1, split
//! 2026-10-05, for an owner who speaks German but wants the model to answer
//! and the voice to speak English): the ASR — a bound session's and dictation's
//! — is told the language the user speaks; the prompt says the user speaks
//! it and asks for the reply language; the TTS is sent the reply language.
//! An unset reply language follows the spoken one, so one language set the
//! old way behaves exactly as before; `auto` works on each field; the new
//! key is checked on every save path. Fakes and the fake GPU world only.

use lmgw_core::ops::settings_set;
use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_attach_kinds::patch;
use crate::chat_voice_language::{dictate, local, speak};
use crate::chat_voice_language_turns::{asr_language, read_out, system};
use crate::chat_voice_settings::{get_json, post as post_json, set_voice};
use crate::chat_voice_speak::{post, stored_reply, thread, world};
use crate::chat_voice_speak_style::get_thread;
use crate::realtime_chat_thread::{say, until_type};
use crate::support::audio_world::tts_row;
use crate::support::audiocpp_gguf;
use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_audio::Asr;
use crate::support::realtime_fakes::Turn;

const TAIL: &str = "unless the user asks for another language.";

#[tokio::test]
async fn speaking_german_and_hearing_english() {
    let g = Gpu::new(24 * GIB, 1, 5).await;
    let root = g.models_dir().join("st");
    std::fs::create_dir_all(&root).unwrap();
    audiocpp_gguf::supertonic(&root);
    store::insert_audio_model(&g.state.db, &tts_row("st", "supertonic"))
        .await
        .unwrap();
    let models = g.models_dir().display().to_string();
    let w = crate::realtime_chat_thread::world_on(g.state.clone(), |s| {
        s.audio.models_dir = models;
        s.chat_tts_alias = "audio/st".into();
        s.realtime.default_voice = "M1".into();
        s.realtime.warm_on_connect = false;
        s.chat_voice_reply_language = "en".into();
    })
    .await;
    // The thread speaks German (`voice.language`), Settings answer in English.
    let tid = w.thread("chatty", json!({})).await;
    let t = w.get(&format!("/chat/api/threads/{tid}")).await;
    let r = &t["thread"]["voice_resolved"];
    assert_eq!(
        r["language"],
        json!({"value": "de", "source": "thread"}),
        "{r}"
    );
    assert_eq!(
        r["reply_language"],
        json!({"value": "en", "source": "chat"}),
        "{r}"
    );
    assert_eq!(
        r["language_notes"],
        json!([]),
        "Supertonic speaks English: {r}"
    );

    let mut ws = w.voice(tid).await;
    w.asr.push(Asr::Text("Wie spät ist es?"));
    w.chat.push(Turn::text(&["It is three o'clock."]));
    say(&mut ws).await;
    until_type(&mut ws, "lmgw.response.timing").await;
    assert_eq!(
        asr_language(&w.asr.seen, 0).as_deref(),
        Some("de"),
        "the ASR hears German"
    );
    let sys = w.chat.seen.chat(0)["messages"][0]["content"].to_string();
    assert!(
        sys.contains(&format!(
            "The user speaks German and hears your reply in an English voice, so answer in \
             English {TAIL}"
        )),
        "{sys}"
    );
    let bodies = g.world().speech_bodies.clone();
    assert!(!bodies.is_empty());
    for b in &bodies {
        assert_eq!(
            b["language"], "en",
            "the voice speaks the reply's language: {b}"
        );
    }

    // Dictation goes by the spoken language too.
    dictate(&w.gw, tid).await;
    assert_eq!(asr_language(&w.asr.seen, 1).as_deref(), Some("de"));
}

#[tokio::test]
async fn one_language_set_the_old_way_drives_all_three_stages_as_before() {
    let l = local(
        &[("st", "supertonic", audiocpp_gguf::supertonic)],
        "audio/st",
        |s| {
            s.realtime.default_voice = "M1".into();
            s.chat_voice_language = "de".into();
        },
    )
    .await;
    let full = get_json(&l.gw, "/api/settings-full").await;
    assert_eq!(full["chat_voice_reply_language"], "", "unset by default");
    let r = l.gw.client();
    let tid = r
        .post(format!("{}/chat/api/threads", l.gw))
        .json(&json!({ "model_alias": "chatty" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let t = get_thread(&l.gw, tid).await;
    let resolved = &t["thread"]["voice_resolved"];
    assert_eq!(
        resolved["language"],
        json!({"value": "de", "source": "chat"})
    );
    assert_eq!(
        resolved["reply_language"],
        json!({"value": "de", "source": "speech_in"}),
        "{resolved}"
    );
    l.chat.push(Turn::text(&["Heute ist es sonnig."]));
    let mid = stored_reply(&l.gw, tid, "hallo").await;
    for b in speak(&l, tid, mid).await {
        assert_eq!(b["language"], "de", "{b}");
    }
}

#[tokio::test]
async fn a_spoken_send_says_the_sentence_for_each_combination() {
    let w = world(|_| {}).await;
    let tid = thread(&w.gw, "chatty").await; // voice.language = de
    let send = |n: usize| {
        let (w, tid) = (&w, tid);
        async move {
            w.chat.push(Turn::text(&["Antwort."]));
            let r = post(
                &w.gw,
                &format!("/chat/api/threads/{tid}/send"),
                json!({ "content": "frag", "speak": true }),
            )
            .await;
            read_out(r).await;
            system(&w.chat, n)
        }
    };
    // Only the spoken language: the sentence from before the split.
    let sys = send(0).await;
    assert!(
        sys.contains(&format!(
            "The user speaks German and hears your reply in a German voice, so answer in German \
             {TAIL}"
        )),
        "{sys}"
    );
    // A reply language of the thread's own.
    let (status, res) = set_voice(
        &w.gw,
        tid,
        json!({ "language": "de", "reply_language": "EN" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["voice"]["reply_language"], "en", "case folded: {res}");
    let sys = send(1).await;
    assert!(
        sys.contains(&format!(
            "The user speaks German and hears your reply in an English voice, so answer in \
             English {TAIL}"
        )),
        "{sys}"
    );
    // Only a reply language: nothing claimed about what the user speaks.
    let (status, _) = set_voice(&w.gw, tid, json!({ "reply_language": "en" })).await;
    assert_eq!(status, 200);
    let sys = send(2).await;
    assert!(
        sys.contains(&format!(
            "The user hears your reply in an English voice, so answer in English {TAIL}"
        )),
        "{sys}"
    );
    assert!(!sys.contains("The user speaks"), "{sys}");
    // Neither: nothing, as before.
    let (status, _) = set_voice(&w.gw, tid, json!({})).await;
    assert_eq!(status, 200);
    let sys = send(3).await;
    assert!(!sys.contains("answer in"), "{sys}");
}

#[tokio::test]
async fn auto_on_each_field() {
    let l = local(
        &[("st", "supertonic", audiocpp_gguf::supertonic)],
        "audio/st",
        |s| {
            s.realtime.default_voice = "M1".into();
            s.chat_voice_language = "de".into();
            s.chat_voice_reply_language = "en".into();
        },
    )
    .await;
    let tid = thread(&l.gw, "chatty").await;
    l.chat.push(Turn::text(&["Sunny."]));
    let mid = stored_reply(&l.gw, tid, "hallo").await;
    for b in speak(&l, tid, mid).await {
        assert_eq!(b["language"], "en", "Settings' reply language: {b}");
    }

    // Reply `auto`: Settings' reply language passed over for the spoken one.
    let (status, res) = set_voice(
        &l.gw,
        tid,
        json!({ "language": "de", "reply_language": "Auto" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(res["voice"]["reply_language"], "auto");
    assert_eq!(
        res["voice_resolved"]["reply_language"],
        json!({"value": "de", "source": "speech_in"})
    );
    for b in speak(&l, tid, mid).await {
        assert_eq!(b["language"], "de", "{b}");
    }

    // Spoken `auto`: the ASR detects; the reply language set in Settings
    // still stands.
    let (status, res) = set_voice(&l.gw, tid, json!({ "language": "auto" })).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(
        res["voice_resolved"]["language"],
        json!({"value": null, "source": "thread"})
    );
    assert_eq!(
        res["voice_resolved"]["reply_language"],
        json!({"value": "en", "source": "chat"})
    );
    for b in speak(&l, tid, mid).await {
        assert_eq!(b["language"], "en", "{b}");
    }

    // Both `auto`: no language anywhere — none sent, the reply follows the
    // user.
    let (status, res) = set_voice(
        &l.gw,
        tid,
        json!({ "language": "auto", "reply_language": "auto" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(
        res["voice_resolved"]["reply_language"],
        json!({"value": null, "source": null})
    );
    for b in speak(&l, tid, mid).await {
        assert!(b.get("language").is_none(), "{b}");
    }

    // A reply language the voice cannot speak: the note says which
    // language it is about while the two differ.
    let (status, res) = set_voice(
        &l.gw,
        tid,
        json!({ "language": "de", "reply_language": "sw" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    let notes = &res["voice_resolved"]["language_notes"];
    assert_eq!(notes.as_array().map(Vec::len), Some(1), "{notes}");
    assert_eq!(notes[0]["stage"], "tts");
    let message = notes[0]["message"].as_str().unwrap();
    assert!(
        message.starts_with(
            "the reply language, sw: text-to-speech model 'audio/st' does not speak sw (sw)"
        ),
        "{message}"
    );
}

#[tokio::test]
async fn the_reply_language_is_checked_on_every_save_path() {
    let (chat, stt, stt2) = crate::chat_voice_settings::mocks().await;
    let (state, gw) = crate::chat_voice_settings::gateway(&chat, &stt, &stt2).await;

    // The self-admin path, read back through both reads.
    let res = settings_set(
        &state,
        patch(json!({ "chat_voice_language": "de", "chat_voice_reply_language": " EN " })),
    )
    .await
    .unwrap();
    assert!(
        res["changed"]
            .as_array()
            .unwrap()
            .contains(&json!("chat_voice_reply_language")),
        "{res}"
    );
    assert_eq!(state.snapshot().settings.chat_voice_reply_language, "en");
    let read = lmgw_core::ops::settings(&state).await.unwrap();
    assert_eq!(read["chat_voice_reply_language"], "en");
    let full = get_json(&gw, "/api/settings-full").await;
    assert_eq!(full["chat_voice_reply_language"], "en");
    for bad in ["eng", "auto", "en-GB"] {
        let e = settings_set(&state, patch(json!({ "chat_voice_reply_language": bad })))
            .await
            .unwrap_err();
        assert!(
            e.contains("chat_voice_reply_language") && e.contains("ISO 639-1"),
            "{bad}: {e}"
        );
    }
    assert_eq!(state.snapshot().settings.chat_voice_reply_language, "en");

    // The dashboard path takes and refuses the same; empty follows the
    // spoken language and is never refused.
    let (status, res) = post_json(
        &gw,
        "/api/op/settings_set_full",
        json!({ "chat_voice_reply_language": "" }),
    )
    .await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(state.snapshot().settings.chat_voice_reply_language, "");
    let (status, res) = post_json(
        &gw,
        "/api/op/settings_set_full",
        json!({ "chat_voice_reply_language": "english" }),
    )
    .await;
    assert_eq!(status, 400, "{res}");

    // A thread's own: strict, by name.
    let tid = crate::chat_attach_kinds::new_thread(&gw, "plain", false).await;
    let (status, res) = set_voice(&gw, tid, json!({ "reply_language": "automatic" })).await;
    assert_eq!(status, 400, "{res}");
    assert!(res.to_string().contains("voice.reply_language"), "{res}");
}

#[test]
fn the_settings_tool_lists_the_reply_language() {
    let (tool, _) = lmgw_core::mcp::selfadmin::full_catalog()
        .into_iter()
        .find(|(t, _)| t["name"] == "lmgw__settings_set")
        .expect("lmgw__settings_set");
    let p = &tool["inputSchema"]["properties"]["chat_voice_reply_language"];
    assert_eq!(p["type"], "string", "{p}");
    assert!(
        p["description"]
            .as_str()
            .unwrap()
            .contains("chat_voice_language"),
        "{p}"
    );
}
