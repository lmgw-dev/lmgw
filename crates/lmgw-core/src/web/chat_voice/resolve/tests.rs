use super::*;
use crate::store::ThreadVoice;

fn snap_with(f: impl FnOnce(&mut crate::config::Settings)) -> Snapshot {
    let mut snap = Snapshot::default();
    f(&mut snap.settings);
    snap
}

fn with_voice(voice: ThreadVoice) -> ChatThread {
    ChatThread {
        voice,
        ..Default::default()
    }
}

#[test]
fn each_field_takes_the_first_level_that_sets_it_and_names_it() {
    let snap = snap_with(|s| {
        s.chat_stt_alias = "chat-asr".into();
        s.realtime.asr_alias = "rt-asr".into();
        s.realtime.tts_alias = "rt-tts".into();
        s.chat_voice = "chat-voice".into();
        s.realtime.default_voice = "rt-voice".into();
        s.realtime.speech_instructions = "calm".into();
        s.chat_voice_language = "de".into();
        s.chat_read_aloud = true;
        s.chat_turn_detection = "server_vad".into();
    });
    let mut t = ChatThread::default();
    let r = resolve(&snap, &t);
    assert_eq!(
        (r.asr.alias.as_deref(), r.asr.source),
        (Some("chat-asr"), Some(Source::Chat))
    );
    assert_eq!(
        (r.tts.alias.as_deref(), r.tts.source),
        (Some("rt-tts"), Some(Source::Realtime))
    );
    // The Chat's voice was chosen for the Chat's model, which is realtime's
    // here (no `chat_tts_alias`): it counts.
    assert_eq!(
        (r.voice.name.as_deref(), r.voice.source),
        (Some("chat-voice"), Some(Source::Chat))
    );
    assert_eq!(
        (r.speech_style.text.as_str(), r.speech_style.source),
        ("calm", Source::Realtime)
    );
    assert_eq!(
        r.language,
        Sourced {
            value: Some("de".into()),
            source: Some(Source::Chat)
        }
    );
    // No reply language set: the reply follows the spoken one.
    assert_eq!(
        r.reply_language,
        Sourced {
            value: Some("de".into()),
            source: Some(Source::SpeechIn)
        }
    );
    assert_eq!(
        r.read_aloud,
        Sourced {
            value: true,
            source: Some(Source::Chat)
        }
    );
    assert_eq!(
        r.turn_detection,
        Sourced {
            value: TurnDetection::ServerVad,
            source: Some(Source::Chat)
        }
    );
    // Without an override, what the thread inherits is what it uses.
    assert_eq!(r.tts.inherited, r.tts.alias);

    t.voice = ThreadVoice {
        asr_alias: Some("own-asr".into()),
        tts_alias: Some("own-tts".into()),
        voice: Some("own-voice".into()),
        speech_style: Some(String::new()),
        language: Some("en".into()),
        reply_language: Some("fr".into()),
        read_aloud: Some(false),
        turn_detection: Some(TurnDetection::PushToTalk),
        audio_input: Some(crate::store::AudioInputMode::Local),
        seed: Some(42),
    };
    let r = resolve(&snap, &t);
    assert_eq!(r.asr.source, Some(Source::Thread));
    assert_eq!(r.tts.alias.as_deref(), Some("own-tts"));
    // What clearing the thread's box would give: the level below.
    assert_eq!(r.asr.inherited.as_deref(), Some("chat-asr"));
    assert_eq!(r.tts.inherited.as_deref(), Some("rt-tts"));
    assert_eq!(r.voice.source, Some(Source::Thread));
    // The thread's "" is none for this thread, not a fall-through.
    assert_eq!(
        (r.speech_style.text.as_str(), r.speech_style.source),
        ("", Source::Thread)
    );
    assert_eq!(r.language.value.as_deref(), Some("en"));
    assert_eq!(r.language.source, Some(Source::Thread));
    assert_eq!(r.reply_language.value.as_deref(), Some("fr"));
    assert_eq!(r.reply_language.source, Some(Source::Thread));
    assert_eq!(
        (r.read_aloud.value, r.read_aloud.source),
        (false, Some(Source::Thread))
    );
    assert_eq!(
        (r.turn_detection.value, r.turn_detection.source),
        (TurnDetection::PushToTalk, Some(Source::Thread))
    );
    assert_eq!(r.seed, Some(42));
}

#[test]
fn the_tts_and_the_style_come_from_the_chat_level() {
    let snap = snap_with(|s| {
        s.chat_tts_alias = "chat-tts".into();
        s.realtime.tts_alias = "rt-tts".into();
        s.chat_speech_style = "warm".into();
        s.realtime.speech_instructions = "calm".into();
    });
    let r = resolve(&snap, &ChatThread::default());
    assert_eq!(
        (r.tts.alias.as_deref(), r.tts.source),
        (Some("chat-tts"), Some(Source::Chat))
    );
    assert_eq!(
        (r.speech_style.text.as_str(), r.speech_style.source),
        ("warm", Source::Chat)
    );
}

#[test]
fn a_thread_tts_override_does_not_take_the_voice_chosen_for_the_chats_model() {
    let snap = snap_with(|s| {
        s.chat_tts_alias = "chat-tts".into();
        s.chat_voice = "alba".into();
    });
    let r = resolve(
        &snap,
        &with_voice(ThreadVoice {
            tts_alias: Some("other-tts".into()),
            ..Default::default()
        }),
    );
    assert_eq!((r.voice.name, r.voice.source), (None, None));
    let note = r.voice.note.expect("the page is told why");
    assert!(
        note.contains("'alba'") && note.contains("'chat-tts'"),
        "{note}"
    );

    // An override naming the same model keeps the voice chosen for it.
    let r = resolve(
        &snap,
        &with_voice(ThreadVoice {
            tts_alias: Some("chat-tts".into()),
            ..Default::default()
        }),
    );
    assert_eq!(
        (r.voice.name.as_deref(), r.voice.source),
        (Some("alba"), Some(Source::Chat))
    );
    // And the thread's own voice always counts.
    let r = resolve(
        &snap,
        &with_voice(ThreadVoice {
            tts_alias: Some("other-tts".into()),
            voice: Some("own".into()),
            ..Default::default()
        }),
    );
    assert_eq!((r.voice.name.as_deref(), r.voice.note), (Some("own"), None));
}

#[test]
fn realtimes_default_voice_is_inherited_never_named() {
    // The Chat speaks with another model than realtime: realtime's default
    // voice is not made the thread's voice. No name goes out, and
    // realtime's chain decides with its own known-voice checks.
    let snap = snap_with(|s| {
        s.chat_tts_alias = "chat-tts".into();
        s.realtime.tts_alias = "rt-tts".into();
        s.realtime.default_voice = "M5".into();
    });
    let r = resolve(&snap, &ChatThread::default());
    assert_eq!(r.voice.name, None, "not explicit");
    assert_eq!(r.voice.inherits.as_deref(), Some("M5"));
    assert_eq!(r.voice.source, Some(Source::Realtime));
    assert_eq!(r.voice.note, None);

    // Realtime's own model too: still inherited, not named.
    let snap = snap_with(|s| {
        s.realtime.tts_alias = "rt-tts".into();
        s.realtime.default_voice = "M5".into();
    });
    let r = resolve(&snap, &ChatThread::default());
    assert_eq!(
        (r.voice.name, r.voice.inherits.as_deref()),
        (None, Some("M5"))
    );

    // A skipped Chat voice still leaves realtime's chain its default.
    let snap = snap_with(|s| {
        s.chat_tts_alias = "chat-tts".into();
        s.chat_voice = "alba".into();
        s.realtime.default_voice = "M5".into();
    });
    let r = resolve(
        &snap,
        &with_voice(ThreadVoice {
            tts_alias: Some("other-tts".into()),
            ..Default::default()
        }),
    );
    assert_eq!(
        (r.voice.name, r.voice.inherits.as_deref()),
        (None, Some("M5"))
    );
    assert!(r.voice.note.is_some());
}

#[test]
fn the_asr_chain_falls_back_to_realtime_and_names_what_is_missing() {
    let snap = snap_with(|s| s.realtime.asr_alias = "rt-asr".into());
    let t = ChatThread::default();
    assert_eq!(asr_alias(&snap, &t).as_deref(), Some("rt-asr"));
    let r = resolve(&snap, &t);
    assert_eq!(r.asr.source, Some(Source::Realtime));
    // Nothing resolves in an empty snapshot: the alias is named as
    // unresolved; the unset TTS is named as not configured.
    let codes: Vec<_> = r.problems.iter().map(|p| (p.stage, p.code)).collect();
    assert_eq!(
        codes,
        vec![("asr", "unresolved"), ("tts", "not_configured")]
    );
    assert!(r.problems[0].message.contains("Settings → Realtime"));

    let none = snap_with(|_| {});
    assert_eq!(asr_alias(&none, &t), None);
    // Blank settings are unset, never an alias named "".
    let blank = snap_with(|s| s.chat_stt_alias = "  ".into());
    assert_eq!(asr_alias(&blank, &t), None);
}

#[test]
fn the_defaults_are_semantic_vad_and_no_read_aloud() {
    let r = resolve(&snap_with(|_| {}), &ChatThread::default());
    assert_eq!(r.turn_detection.value, TurnDetection::SemanticVad);
    assert!(!r.read_aloud.value);
    assert_eq!(
        r.language,
        Sourced {
            value: None,
            source: None
        }
    );
    assert_eq!(
        (r.voice.name, r.voice.source, r.voice.inherits),
        (None, None, None)
    );
    assert_eq!(
        (r.speech_style.text.as_str(), r.speech_style.source),
        ("", Source::Realtime)
    );
}

#[test]
fn realtime_is_refused_for_admin_chat_only_and_flags_admin_tools() {
    let mut snap = snap_with(|s| s.self_admin = SelfAdmin::Full);
    let admin = ChatThread {
        kind: "admin".into(),
        ..Default::default()
    };
    let r = realtime_availability(&snap, &admin);
    assert!(!r.ok);
    assert_eq!(r.code, Some("chat_thread_admin"));
    assert!(r.reason.unwrap().contains("Admin Chat"));

    let labelled = ChatThread {
        kind: "chat".into(),
        mcp_tools: vec![crate::store::ThreadMcp {
            server_label: crate::mcp::exec::SELF_ADMIN_LABEL.into(),
            allowed_tools: None,
        }],
        ..Default::default()
    };
    let r = realtime_availability(&snap, &labelled);
    assert!(r.ok && r.admin_tools);
    assert_eq!(r.code, None);
    snap.settings.self_admin = SelfAdmin::ReadOnly;
    assert!(!realtime_availability(&snap, &labelled).admin_tools);
}
