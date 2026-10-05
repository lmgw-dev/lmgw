//! A thread's speech on a voice-design row (chat-voice design §6.1; WP4
//! review m2, m7): the owner-wide speech style stands back for a row that
//! describes itself, as realtime's own setting does, while the thread's own
//! style wins; the thread JSON shows the style the speech uses; and the
//! press's warm draws the thread's seed, so it warms the voice read-aloud
//! then speaks with. The GPU world fakes the row's container.

use lmgw_core::config::Settings;
use lmgw_core::store;
use serde_json::{json, Value};

use crate::chat_voice_speak::{settings, stored_reply, thread, Reader};
use crate::common::{serve, Gw};
use crate::support::audio_world::{tts_row, wav_bytes};
use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_fakes::{add_chat_aliases, chat_fake, ChatFake, Turn};

/// A gateway on the GPU world whose Chat speaks with a voice-design row,
/// `audio/design-row` (one container), described by `instruct` when given;
/// realtime's default voice is `alba`. Then `tweak`.
pub(crate) struct Design {
    pub g: Gpu,
    pub chat: ChatFake,
    pub gw: Gw,
}

pub(crate) async fn design_world(
    instruct: Option<&str>,
    tweak: impl FnOnce(&mut Settings),
) -> Design {
    let g = Gpu::new(24 * GIB, 1, 5).await;
    let chat = chat_fake().await;
    add_chat_aliases(&g.state, &chat).await;
    let root = g.models_dir().join("design-row");
    std::fs::create_dir_all(&root).unwrap();
    crate::realtime_expressive::voice_design(&root);
    let mut row = tts_row("design-row", "qwen3_tts");
    row.task = "vdes".into();
    if let Some(instruct) = instruct {
        row.default_request_options = json!({ "instruct": instruct })
            .as_object()
            .cloned()
            .unwrap();
    }
    store::insert_audio_model(&g.state.db, &row).await.unwrap();
    let voices = g.models_dir().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("alba.wav"), wav_bytes(24_000, 480)).unwrap();
    let models = g.models_dir().display().to_string();
    settings(&g.state, |s| {
        s.audio.models_dir = models;
        s.chat_tts_alias = "audio/design-row".into();
        s.realtime.default_voice = "alba".into();
        tweak(s);
    })
    .await;
    let gw = serve(g.state.clone()).await;
    Design { g, chat, gw }
}

pub(crate) async fn get_thread(gw: &Gw, tid: i64) -> Value {
    gw.client()
        .get(format!("{gw}/chat/api/threads/{tid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Read reply `mid` aloud to its end; the speech bodies it sent.
async fn speak(d: &Design, tid: i64, mid: i64) -> Vec<Value> {
    let before = d.g.world().speech_bodies.len();
    let r =
        d.gw.client()
            .post(format!(
                "{}/chat/api/threads/{tid}/messages/{mid}/speak",
                d.gw
            ))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
    let events = Reader::new(r).rest().await;
    assert_eq!(events.last().unwrap().0, "speech_done", "{events:?}");
    d.g.world().speech_bodies[before..].to_vec()
}

#[tokio::test]
async fn the_owner_s_style_stands_back_for_a_row_that_describes_itself() {
    let d = design_world(Some("a deep, slow narrator"), |s| {
        s.chat_speech_style = "calm, friendly".into();
    })
    .await;
    let tid = thread(&d.gw, "chatty").await;
    d.chat.push(Turn::text(&["Guten Tag."]));
    let mid = stored_reply(&d.gw, tid, "hallo").await;

    // The row designs its voice from its own description: Settings → Chat's
    // style is not a voice, and the page is shown what is used.
    let t = get_thread(&d.gw, tid).await;
    assert_eq!(
        t["thread"]["voice_resolved"]["speech_style"],
        json!({ "text": "a deep, slow narrator", "source": "row" })
    );
    let bodies = speak(&d, tid, mid).await;
    assert!(!bodies.is_empty());
    for b in &bodies {
        assert!(!b.to_string().contains("calm, friendly"), "{b}");
    }

    // The thread's own style is the owner's choice for it: it wins.
    let r =
        d.gw.client()
            .post(format!("{}/chat/api/threads/{tid}/settings", d.gw))
            .json(&json!({ "voice": { "language": "de", "speech_style": "whispering" } }))
            .send()
            .await
            .unwrap();
    let answer: Value = r.json().await.unwrap();
    assert_eq!(
        answer["voice_resolved"]["speech_style"],
        json!({ "text": "whispering", "source": "thread" })
    );
    let bodies = speak(&d, tid, mid).await;
    assert!(!bodies.is_empty());
    for b in &bodies {
        assert!(b.to_string().contains("whispering"), "{b}");
    }
}

#[tokio::test]
async fn a_row_with_no_description_takes_the_owner_s_style() {
    let d = design_world(None, |s| s.chat_speech_style = "calm, friendly".into()).await;
    let tid = thread(&d.gw, "chatty").await;
    d.chat.push(Turn::text(&["Guten Tag."]));
    let mid = stored_reply(&d.gw, tid, "hallo").await;
    let t = get_thread(&d.gw, tid).await;
    assert_eq!(
        t["thread"]["voice_resolved"]["speech_style"],
        json!({ "text": "calm, friendly", "source": "chat" })
    );
    let bodies = speak(&d, tid, mid).await;
    assert!(!bodies.is_empty());
    for b in &bodies {
        assert!(b.to_string().contains("calm, friendly"), "{b}");
    }
}

/// Review m7: the press's `tts` stage draws the thread's seed on its first
/// use, as a read-aloud would, so a row whose voice comes from its seed
/// warms the voice the read-aloud then speaks with.
#[tokio::test]
async fn the_press_warms_the_voice_the_read_aloud_speaks_with() {
    let d = design_world(Some("a deep, slow narrator"), |_| {}).await;
    let tid = thread(&d.gw, "chatty").await;
    d.chat.push(Turn::text(&["Guten Tag."]));
    let mid = stored_reply(&d.gw, tid, "hallo").await;
    let t = get_thread(&d.gw, tid).await;
    assert_eq!(t["thread"]["voice_resolved"]["seed"], Value::Null);

    let r =
        d.gw.client()
            .post(format!("{}/chat/api/threads/{tid}/voice/warm", d.gw))
            .json(&json!({ "stages": ["tts"] }))
            .send()
            .await
            .unwrap();
    let events = Reader::new(r).rest().await;
    assert_eq!(events.last().unwrap().0, "done", "{events:?}");
    let t = get_thread(&d.gw, tid).await;
    let seed = t["thread"]["voice_resolved"]["seed"]
        .as_u64()
        .unwrap_or_else(|| panic!("drawn by the press: {t}"));
    let warmed = d.g.world().speech_bodies.clone();
    assert_eq!(warmed.len(), 1, "{warmed:?}");
    assert_eq!(warmed[0]["seed"], json!(seed), "{warmed:?}");

    let bodies = speak(&d, tid, mid).await;
    assert!(!bodies.is_empty());
    assert!(
        bodies.iter().all(|b| b["seed"] == json!(seed)),
        "{bodies:?}"
    );
}
