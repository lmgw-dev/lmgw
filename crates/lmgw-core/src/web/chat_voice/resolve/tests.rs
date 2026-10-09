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
        audio_input: Some(crate::store::AudioInputMode::On),
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
            require_approval: None,
        }],
        ..Default::default()
    };
    let r = realtime_availability(&snap, &labelled);
    assert!(r.ok && r.admin_tools);
    assert_eq!(r.code, None);
    snap.settings.self_admin = SelfAdmin::ReadOnly;
    assert!(!realtime_availability(&snap, &labelled).admin_tools);
}

// -- personality profiles (design D8, §2.3) ---------------------------------

use crate::config::chat_profile::ProfileVoice;

fn profile(id: i64, voice: ProfileVoice) -> ChatProfile {
    ChatProfile {
        id,
        name: "Ruhig".into(),
        voice,
        ..Default::default()
    }
}

fn pv(tts: Option<&str>, voice: Option<&str>, style: Option<&str>) -> ProfileVoice {
    ProfileVoice {
        tts_alias: tts.map(Into::into),
        voice: voice.map(Into::into),
        speech_style: style.map(Into::into),
    }
}

fn bound_to(id: i64, voice: ThreadVoice) -> ChatThread {
    ChatThread {
        profile_id: Some(id),
        voice,
        ..Default::default()
    }
}

fn chat_snap(profiles: Vec<ChatProfile>) -> Snapshot {
    let mut snap = snap_with(|s| {
        s.chat_tts_alias = "chat-tts".into();
        s.chat_voice = "alba".into();
        s.chat_speech_style = "warm".into();
        s.realtime.tts_alias = "rt-tts".into();
        s.realtime.default_voice = "M5".into();
    });
    snap.chat_profiles = profiles;
    snap
}

#[test]
fn a_profile_sits_between_the_thread_and_the_chat_for_each_field() {
    let snap = chat_snap(vec![profile(
        1,
        pv(Some("p-tts"), Some("pv"), Some("slow")),
    )]);
    let r = resolve(&snap, &bound_to(1, ThreadVoice::default()));
    assert_eq!(
        (
            r.tts.alias.as_deref(),
            r.tts.source,
            r.tts.inherited.as_deref()
        ),
        (Some("p-tts"), Some(Source::Profile), Some("chat-tts"))
    );
    assert_eq!(
        (r.voice.name.as_deref(), r.voice.source, r.voice.note),
        (Some("pv"), Some(Source::Profile), None)
    );
    assert_eq!(
        (r.speech_style.text.as_str(), r.speech_style.source),
        ("slow", Source::Profile)
    );

    // The thread's own fields win, field by field.
    let r = resolve(
        &snap,
        &bound_to(
            1,
            ThreadVoice {
                speech_style: Some("fast".into()),
                voice: Some("own".into()),
                ..Default::default()
            },
        ),
    );
    assert_eq!(r.tts.source, Some(Source::Profile), "the thread set no TTS");
    assert_eq!(r.voice.source, Some(Source::Thread));
    assert_eq!(r.speech_style.source, Source::Thread);

    // A profile that sets none of the three leaves the Chat's.
    let snap = chat_snap(vec![profile(1, ProfileVoice::default())]);
    let r = resolve(&snap, &bound_to(1, ThreadVoice::default()));
    assert_eq!(r, resolve(&snap, &ChatThread::default()));
    // So does an id that names no profile (design §2.1).
    assert_eq!(
        resolve(&snap, &bound_to(9, ThreadVoice::default())),
        resolve(&snap, &ChatThread::default())
    );
}

#[test]
fn a_profile_style_of_nothing_is_a_value_that_silences_the_chat_style() {
    let snap = chat_snap(vec![profile(1, pv(None, None, Some("")))]);
    let r = resolve(&snap, &bound_to(1, ThreadVoice::default()));
    assert_eq!(
        (r.speech_style.text.as_str(), r.speech_style.source),
        ("", Source::Profile)
    );
}

#[test]
fn a_profile_voice_belongs_to_the_profiles_model_or_the_chats_own() {
    // No TTS in the profile: its voice was chosen for the Chat's model.
    let snap = chat_snap(vec![profile(1, pv(None, Some("pv"), None))]);
    let r = resolve(&snap, &bound_to(1, ThreadVoice::default()));
    assert_eq!(
        (r.voice.name.as_deref(), r.voice.source),
        (Some("pv"), Some(Source::Profile))
    );
    // The thread speaks with another model: the voice stands back, with a
    // note naming the profile, and neither does the Chat's own take it.
    let other = ThreadVoice {
        tts_alias: Some("other-tts".into()),
        ..Default::default()
    };
    let r = resolve(&snap, &bound_to(1, other.clone()));
    // Only realtime's default is reported, as what its chain starts from.
    assert_eq!(
        (r.voice.name.as_deref(), r.voice.source),
        (None, Some(Source::Realtime))
    );
    assert_eq!(r.voice.inherits.as_deref(), Some("M5"));
    let note = r.voice.note.expect("the page is told why");
    assert!(
        note.contains("'pv'") && note.contains("'Ruhig'") && note.contains("'chat-tts'"),
        "{note}"
    );
    assert!(note.contains("'alba'"), "both voices are explained: {note}");
    // An override naming the profile's model keeps it.
    let same = ThreadVoice {
        tts_alias: Some("chat-tts".into()),
        ..Default::default()
    };
    let r = resolve(&snap, &bound_to(1, same));
    assert_eq!(r.voice.source, Some(Source::Profile));

    // A profile with its own TTS: its voice follows that model, and the
    // Chat's voice is not taken for it (the model is not the Chat's).
    let snap = chat_snap(vec![profile(1, pv(Some("p-tts"), None, None))]);
    let r = resolve(&snap, &bound_to(1, ThreadVoice::default()));
    assert_eq!(r.voice.name, None, "alba was chosen for chat-tts");
    assert!(r.voice.note.is_some());
    let snap = chat_snap(vec![profile(1, pv(Some("p-tts"), Some("pv"), None))]);
    assert_eq!(
        resolve(&snap, &bound_to(1, other)).voice.name,
        None,
        "the thread's model is not the profile's"
    );
    // The thread's own voice always counts.
    let own = ThreadVoice {
        tts_alias: Some("other-tts".into()),
        voice: Some("own".into()),
        ..Default::default()
    };
    let r = resolve(&snap, &bound_to(1, own));
    assert_eq!((r.voice.name.as_deref(), r.voice.note), (Some("own"), None));
}

#[test]
fn a_profile_tts_that_does_not_resolve_is_a_problem_naming_the_profile() {
    let snap = chat_snap(vec![profile(1, pv(Some("gone"), None, None))]);
    let r = resolve(&snap, &bound_to(1, ThreadVoice::default()));
    let p = r.problems.iter().find(|p| p.stage == "tts").unwrap();
    assert_eq!(p.code, "unresolved");
    assert!(p.message.contains("personality profile"), "{}", p.message);
}
