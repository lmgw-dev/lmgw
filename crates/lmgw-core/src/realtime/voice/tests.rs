//! The voice chain's rules, on facts given directly (realtime design §5.3).

use super::*;

fn facts(listed: Option<&[&str]>) -> VoiceFacts {
    VoiceFacts {
        listed: listed.map(|l| l.iter().map(|s| s.to_string()).collect()),
        seen: None,
        presets: vec!["narrator".into()],
        clip_presets: Vec::new(),
        default_preset: None,
        library: Some(vec!["me_11s".into()]),
        native: Vec::new(),
        designs: false,
        openai: false,
        unvoiced: Unvoiced::Unknown,
        untranscribed: Vec::new(),
    }
}

fn name(n: &str) -> Voice {
    Voice::Name(n.into())
}

fn resolved(d: Decision) -> (Option<String>, String, VoiceVia) {
    match d {
        Decision::Outcome(VoiceOutcome::Resolved(v)) => (v.send, v.name, v.via),
        other => panic!("expected a voice, got {other:?}"),
    }
}

#[test]
fn the_chain_in_order() {
    let mut s = RealtimeSettings::default();
    s.voice_map.insert("alloy".into(), "alba".into());
    s.voice_map.insert("boss".into(), "cosette".into());
    let f = facts(Some(&["alba", "alloy", "cosette"]));
    // 1: the model's own name wins over a mapping of it.
    assert_eq!(
        resolved(decide("t", &name("alloy"), &f, &s)).2,
        VoiceVia::Model
    );
    assert_eq!(
        resolved(decide("t", &name("narrator"), &f, &s)).2,
        VoiceVia::Model
    );
    assert_eq!(
        resolved(decide("t", &name("me_11s"), &f, &s)).2,
        VoiceVia::Model
    );
    // 2: mapped.
    let (send, n, via) = resolved(decide("t", &name("boss"), &f, &s));
    assert_eq!(
        (send.as_deref(), n.as_str(), via),
        (Some("cosette"), "cosette", VoiceVia::VoiceMap)
    );
    // 3: a built-in the model lacks, with nothing configured: missing.
    assert!(matches!(
        decide("t", &name("marin"), &f, &s),
        Decision::Outcome(VoiceOutcome::Missing(_))
    ));
    s.default_voice = "alba".into();
    assert_eq!(
        resolved(decide("t", &name("marin"), &f, &s)),
        (Some("alba".into()), "alba".into(), VoiceVia::DefaultVoice)
    );
    // 4: a library clip by id.
    let id = Voice::Id {
        id: "me_11s".into(),
    };
    assert_eq!(resolved(decide("t", &id, &f, &s)).2, VoiceVia::Library);
    let missing = Voice::Id { id: "nope".into() };
    assert!(matches!(
        decide("t", &missing, &f, &s),
        Decision::NotFound(_)
    ));
    // 5: anything else.
    assert!(matches!(
        decide("t", &name("zork"), &f, &s),
        Decision::NotFound(_)
    ));
    // …unless the model's list is unread: then it is provisional, checked
    // at the first spoken clause (WP3 review M1).
    let Decision::Outcome(VoiceOutcome::Resolved(v)) = decide("t", &name("zork"), &facts(None), &s)
    else {
        panic!("a provisional voice");
    };
    assert_eq!((v.send.as_deref(), v.verified), (Some("zork"), false));
}

#[test]
fn a_read_list_decides_every_rule() {
    // Once a response has read the list, a mapping's target or a default
    // the list does not show is no longer provisional (package B review
    // 5) — and, being the owner's setting, missing, not "not found": the
    // client's session.update is not refused for it (B2 review 4).
    let mut s = RealtimeSettings {
        default_voice: "ghost".into(),
        ..Default::default()
    };
    s.voice_map.insert("boss".into(), "phantom".into());
    let listed = facts(Some(&["alba"]));
    for asked in ["boss", "marin"] {
        let Decision::Outcome(VoiceOutcome::Missing(why)) = decide("t", &name(asked), &listed, &s)
        else {
            panic!("{asked}: missing");
        };
        assert!(why.contains("named by the setting realtime."), "{why}");
        assert!(why.contains("— fix the setting realtime."), "{why}");
        assert!(matches!(
            resolve(Some("t"), &name(asked), &listed, &s),
            Ok(VoiceOutcome::Missing(_))
        ));
    }
    // A default preset the list does not show is the row's to fix: missing.
    let mut preset = facts(Some(&["alba"]));
    preset.default_preset = Some(Value::String("narrator-gone".into()));
    let plain = RealtimeSettings::default();
    assert!(matches!(
        decide("t", &name("marin"), &preset, &plain),
        Decision::Outcome(VoiceOutcome::Missing(_))
    ));
    // The client's own name the list does not show is not found.
    let e = resolve(Some("t"), &name("zork"), &listed, &s).unwrap_err();
    assert_eq!(e.code.as_deref(), Some("voice_not_found"));
    // Unread, the same names are provisional.
    let unread = facts(None);
    for asked in ["boss", "marin"] {
        assert!(matches!(
            decide("t", &name(asked), &unread, &s),
            Decision::Outcome(VoiceOutcome::Resolved(SpeakVoice {
                verified: false,
                ..
            }))
        ));
    }
}

#[test]
fn a_fallback_s_engine_is_its_protocol_and_kind() {
    use crate::config::{Protocol, UpstreamKind};
    assert_eq!(
        Engine::of(Protocol::Openai, UpstreamKind::AudioCpp),
        Engine::AudioCpp
    );
    assert_eq!(
        Engine::of(Protocol::Openai, UpstreamKind::Generic),
        Engine::OpenAi
    );
    for (p, k) in [
        (Protocol::LlamaCpp, UpstreamKind::LlamaServer),
        (Protocol::Openai, UpstreamKind::SdCpp),
        (Protocol::Gemini, UpstreamKind::Generic),
    ] {
        assert!(matches!(Engine::of(p, k), Engine::Other(_)), "{p:?} {k:?}");
    }
}

#[test]
fn an_audio_cpp_fallback_tries_every_rule_in_turn() {
    // The session maps alloy to the primary's own voice; the fallback lacks
    // it, so the chain goes on (package B review 7).
    let mut s = RealtimeSettings {
        default_voice: "primary_default".into(),
        ..Default::default()
    };
    s.voice_map.insert("alloy".into(), "alba".into());
    let mut theirs = VoiceFacts {
        listed: Some(vec!["javert".into()]),
        presets: vec!["narrator".into()],
        ..Default::default()
    };
    let speaks = |f: &VoiceFacts, s: &RealtimeSettings| {
        let v = for_fallback("speak", "cloud", &name("alloy"), Some(f), s).unwrap();
        (v.send, v.via)
    };
    // Neither the mapping nor the default voice, nor a preset: refused,
    // naming what was tried.
    let e = for_fallback("speak", "cloud", &name("alloy"), Some(&theirs), &s).unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("'alba' (the setting realtime.voice_map)")
            && msg.contains("'primary_default' (the setting realtime.default_voice)"),
        "{msg}"
    );
    // The fallback row's named default preset.
    theirs.default_preset = Some(Value::String("narrator".into()));
    assert_eq!(
        speaks(&theirs, &s),
        (Some("narrator".into()), VoiceVia::DefaultPreset)
    );
    // A default voice it has comes before its preset.
    s.default_voice = "javert".into();
    assert_eq!(
        speaks(&theirs, &s),
        (Some("javert".into()), VoiceVia::DefaultVoice)
    );
    // An inline preset speaks last, sent as no voice.
    s.default_voice = String::new();
    theirs.default_preset = Some(serde_json::json!({"voice_id": "bob"}));
    assert_eq!(speaks(&theirs, &s), (None, VoiceVia::DefaultPreset));
    // A mapping it has wins.
    theirs.listed = Some(vec!["alba".into()]);
    assert_eq!(
        speaks(&theirs, &s),
        (Some("alba".into()), VoiceVia::VoiceMap)
    );
}

#[test]
fn only_what_the_facts_show_is_verified() {
    let mut s = RealtimeSettings {
        default_voice: "alba".into(),
        ..Default::default()
    };
    s.voice_map.insert("boss".into(), "cosette".into());
    let verified = |d: Decision| match d {
        Decision::Outcome(VoiceOutcome::Resolved(v)) => v.verified,
        other => panic!("expected a voice, got {other:?}"),
    };
    let unread = facts(None);
    let listed = facts(Some(&["alba", "cosette"]));
    // A preset or a library clip is known without the list.
    assert!(verified(decide("t", &name("narrator"), &unread, &s)));
    assert!(verified(decide("t", &name("me_11s"), &unread, &s)));
    // A mapping's target and the default voice are names like any other
    // (m10): unverified until a list shows them.
    assert!(!verified(decide("t", &name("boss"), &unread, &s)));
    assert!(!verified(decide("t", &name("marin"), &unread, &s)));
    assert!(verified(decide("t", &name("boss"), &listed, &s)));
    assert!(verified(decide("t", &name("marin"), &listed, &s)));
    // Resolution never asks the model: the session resolves from the facts.
    let o = resolve(Some("t"), &name("zork"), &unread, &s).unwrap();
    assert!(matches!(
        o,
        VoiceOutcome::Resolved(SpeakVoice {
            verified: false,
            ..
        })
    ));
    let e = resolve(Some("t"), &name("zork"), &listed, &s).unwrap_err();
    assert_eq!(e.code.as_deref(), Some("voice_not_found"));
}

#[test]
fn a_fallback_speaks_its_own_voice_or_says_it_has_none() {
    use crate::error::GatewayError;
    let mut s = RealtimeSettings {
        default_voice: "alba".into(),
        ..Default::default()
    };
    s.voice_map.insert("boss".into(), "ash".into());
    let code = |e: GatewayError| match e {
        GatewayError::Refused { code, message, .. } => {
            assert!(message.contains("fallback 'cloud'"), "{message}");
            code
        }
        other => panic!("{other:?}"),
    };
    // OpenAI-kind: the built-in name itself, not the local default voice.
    let v = for_fallback("speak", "cloud", &name("Marin"), None, &s).unwrap();
    assert_eq!(v.send.as_deref(), Some("marin"));
    let v = for_fallback("speak", "cloud", &name("boss"), None, &s).unwrap();
    assert_eq!(v.send.as_deref(), Some("ash"));
    for asked in [
        name("cosette"),
        Voice::Id {
            id: "me_11s".into(),
        },
    ] {
        let e = for_fallback("speak", "cloud", &asked, None, &s).unwrap_err();
        assert_eq!(code(e), "voice_not_configured");
    }
    // audio.cpp: the chain over the fallback's own list.
    let theirs = VoiceFacts {
        listed: Some(vec!["alba".into(), "javert".into()]),
        ..Default::default()
    };
    let v = for_fallback("speak", "cloud", &name("javert"), Some(&theirs), &s).unwrap();
    assert_eq!(v.send.as_deref(), Some("javert"));
    let v = for_fallback("speak", "cloud", &name("marin"), Some(&theirs), &s).unwrap();
    assert_eq!(
        v.send.as_deref(),
        Some("alba"),
        "realtime.default_voice, which it has"
    );
    let e = for_fallback("speak", "cloud", &name("cosette"), Some(&theirs), &s).unwrap_err();
    assert_eq!(code(e), "voice_not_configured");
    s.default_voice = "cosette".into();
    let e = for_fallback("speak", "cloud", &name("marin"), Some(&theirs), &s).unwrap_err();
    assert_eq!(
        code(e),
        "voice_not_configured",
        "a default it does not have"
    );
    // A list that cannot be read refuses nothing (B2 review 8): presets
    // and the library still answer, then the inline preset, and otherwise
    // the voice the primary's chain names goes unverified — the engine
    // judges.
    let unread = VoiceFacts::default();
    let v = for_fallback("speak", "cloud", &name("javert"), Some(&unread), &s).unwrap();
    assert_eq!((v.send.as_deref(), v.verified), (Some("javert"), false));
    let v = for_fallback("speak", "cloud", &name("marin"), Some(&unread), &s).unwrap();
    assert_eq!(
        (v.send.as_deref(), v.via, v.verified),
        (Some("cosette"), VoiceVia::DefaultVoice, false)
    );
    let v = for_fallback("speak", "cloud", &name("boss"), Some(&unread), &s).unwrap();
    assert_eq!(v.send.as_deref(), Some("ash"), "the mapping's target");
    // An OpenAI name with a mapping: its target before the default, in the
    // primary chain's order (B4 review) — it used to skip to the default.
    let mut mapped = s.clone();
    mapped.voice_map.insert("marin".into(), "fantine".into());
    let v = for_fallback("speak", "cloud", &name("marin"), Some(&unread), &mapped).unwrap();
    assert_eq!(
        (v.send.as_deref(), v.via, v.verified),
        (Some("fantine"), VoiceVia::VoiceMap, false)
    );
    let presets = VoiceFacts {
        presets: vec!["javert".into()],
        ..Default::default()
    };
    let v = for_fallback("speak", "cloud", &name("javert"), Some(&presets), &s).unwrap();
    assert!(v.verified, "a preset shows it");
    let inline = VoiceFacts {
        default_preset: Some(serde_json::json!({"voice_id": "anna"})),
        ..Default::default()
    };
    let mut plain = s.clone();
    plain.default_voice = String::new();
    let v = for_fallback("speak", "cloud", &name("marin"), Some(&inline), &plain).unwrap();
    assert_eq!((v.send, v.name.as_str()), (None, "anna"));
    let clip = Voice::Id { id: "nope".into() };
    let e = for_fallback("speak", "cloud", &clip, Some(&unread), &s).unwrap_err();
    assert_eq!(code(e), "voice_not_configured", "a clip the library lacks");
}

#[test]
fn the_rows_default_preset_after_the_setting() {
    let s = RealtimeSettings::default();
    let mut f = facts(Some(&[]));
    f.default_preset = Some(Value::String("narrator".into()));
    assert_eq!(
        resolved(decide("t", &name("alloy"), &f, &s)),
        (
            Some("narrator".into()),
            "narrator".into(),
            VoiceVia::DefaultPreset
        )
    );
    // An inline preset speaks when the field is left out, and is named
    // by what it loads.
    f.default_preset = Some(serde_json::json!({"voice_ref": "/models/voices/anna.wav"}));
    assert_eq!(
        resolved(decide("t", &name("alloy"), &f, &s)),
        (None, "anna".into(), VoiceVia::DefaultPreset)
    );
}

#[test]
fn a_shipped_voice_is_known_in_any_case_and_verified() {
    let f = VoiceFacts {
        native: vec!["Jason".into(), "ryan".into()],
        ..Default::default()
    };
    assert!(f.knows("Ryan") && f.knows("jason") && !f.knows("alba"));
    let s = RealtimeSettings::default();
    match decide("t", &name("Ryan"), &f, &s) {
        Decision::Outcome(VoiceOutcome::Resolved(v)) => {
            assert!(v.verified, "known from the package, not provisional");
            assert_eq!(v.send.as_deref(), Some("Ryan"), "shaping spells it");
        }
        other => panic!("{other:?}"),
    }
}

/// A voice-design TTS reads no speaker: an OpenAI name nobody gave a voice
/// to stand for is sent as none, named `designed` (R2) — not "missing",
/// which refused every audio response on default settings. A voice the
/// client or the owner names is sent as before.
#[test]
fn a_designed_voice_is_sent_none_unless_someone_names_one() {
    let mut f = facts(Some(&["alba"]));
    f.designs = true;
    let s = RealtimeSettings::default();
    let Decision::Outcome(VoiceOutcome::Resolved(v)) = decide("t", &name("marin"), &f, &s) else {
        panic!("a designed voice");
    };
    assert_eq!(
        (v.send, v.name.as_str(), v.via, v.verified),
        (None, DESIGNED, VoiceVia::Designed, true)
    );
    // Unread, the same: nothing to check at the first clause.
    let mut unread = facts(None);
    unread.designs = true;
    assert_eq!(
        resolved(decide("t", &name("alloy"), &unread, &s)),
        (None, DESIGNED.into(), VoiceVia::Designed)
    );
    // The client's own voice, the owner's default voice or default preset:
    // sent.
    assert_eq!(
        resolved(decide("t", &name("alba"), &f, &s)).0.as_deref(),
        Some("alba")
    );
    let owner = RealtimeSettings {
        default_voice: "alba".into(),
        ..Default::default()
    };
    assert_eq!(
        resolved(decide("t", &name("marin"), &f, &owner)),
        (Some("alba".into()), "alba".into(), VoiceVia::DefaultVoice)
    );
    let mut preset = f.clone();
    preset.default_preset = Some(Value::String("narrator".into()));
    assert_eq!(
        resolved(decide("t", &name("marin"), &preset, &s)).2,
        VoiceVia::DefaultPreset
    );
    // A name of the client's the list rules out is still not found.
    assert!(matches!(
        decide("t", &name("zork"), &f, &s),
        Decision::NotFound(_)
    ));
}

/// A cloud OpenAI TTS as the primary (live run 3, D2): its voices are
/// OpenAI's names. `alloy` went to rule 3 — `voice_not_configured` with no
/// `default_voice`, and the owner's local `alba` sent to OpenAI with one.
/// The name is the model's own, as for an OpenAI fallback.
#[test]
fn an_openai_tts_speaks_the_openai_name_it_is_asked_for() {
    let f = VoiceFacts {
        openai: true,
        ..Default::default()
    };
    let owner = RealtimeSettings {
        default_voice: "alba".into(),
        ..Default::default()
    };
    for s in [RealtimeSettings::default(), owner.clone()] {
        let Decision::Outcome(VoiceOutcome::Resolved(v)) = decide("t", &name("alloy"), &f, &s)
        else {
            panic!("alloy is the model's own");
        };
        assert_eq!(
            (v.send.as_deref(), v.via, v.verified),
            (Some("alloy"), VoiceVia::Model, true)
        );
        // The session's own default, and any spelling.
        assert_eq!(
            resolved(decide("t", &name("marin"), &f, &s)).0.as_deref(),
            Some("marin")
        );
        assert_eq!(
            resolved(decide("t", &name("Cedar"), &f, &s)).0.as_deref(),
            Some("cedar")
        );
    }
    // A mapping onto one of its names is verified; any other name is the
    // engine's to judge, as before.
    let mut mapped = owner;
    mapped.voice_map.insert("narrator".into(), "Sage".into());
    let Decision::Outcome(VoiceOutcome::Resolved(v)) = decide("t", &name("narrator"), &f, &mapped)
    else {
        panic!("mapped");
    };
    assert_eq!((v.send.as_deref(), v.verified), (Some("sage"), true));
    let Decision::Outcome(VoiceOutcome::Resolved(v)) = decide("t", &name("zork"), &f, &mapped)
    else {
        panic!("provisional");
    };
    assert_eq!((v.send.as_deref(), v.verified), (Some("zork"), false));
    // Not an OpenAI TTS: an OpenAI name is still rule 3's.
    let local = VoiceFacts::default();
    assert_eq!(
        resolved(decide("t", &name("alloy"), &local, &mapped)),
        (Some("alba".into()), "alba".into(), VoiceVia::DefaultVoice)
    );
}

/// Live run 3, N2: the owner's `default_voice` is a voice of another model
/// (Pocket's `alba`); a session that switched to a voice-design TTS was
/// refused `voice_not_configured` for it. A designing TTS passes over a
/// default voice it is not known to have; one it knows is still sent.
#[test]
fn a_designing_tts_passes_over_a_default_voice_it_does_not_have() {
    let owner = RealtimeSettings {
        default_voice: "alba".into(),
        ..Default::default()
    };
    let mut unread = facts(None);
    unread.designs = true;
    assert_eq!(
        resolved(decide("t", &name("marin"), &unread, &owner)),
        (None, DESIGNED.into(), VoiceVia::Designed)
    );
    // Its list, read, without it: the same.
    let mut listed = facts(Some(&["narrator"]));
    listed.designs = true;
    assert_eq!(
        resolved(decide("t", &name("marin"), &listed, &owner)).2,
        VoiceVia::Designed
    );
    // A row's own default preset still comes before the design.
    let mut preset = unread.clone();
    preset.default_preset = Some(Value::String("narrator".into()));
    assert_eq!(
        resolved(decide("t", &name("marin"), &preset, &owner)).2,
        VoiceVia::DefaultPreset
    );
    // A default voice it knows — a library clip, a preset, its list — is sent.
    for known in ["me_11s", "narrator"] {
        let s = RealtimeSettings {
            default_voice: known.into(),
            ..Default::default()
        };
        assert_eq!(
            resolved(decide("t", &name("marin"), &unread, &s)),
            (Some(known.into()), known.into(), VoiceVia::DefaultVoice)
        );
    }
    // A TTS that does not design keeps the owner's word, unverified.
    let plain = facts(None);
    let Decision::Outcome(VoiceOutcome::Resolved(v)) = decide("t", &name("marin"), &plain, &owner)
    else {
        panic!("provisional");
    };
    assert_eq!((v.send.as_deref(), v.verified), (Some("alba"), false));
}

/// R3: a TTS whose engine speaks with a fixed voice of its own when none is
/// named (Magpie, Kokoro, Supertonic — `audio::families::unvoiced`) is sent
/// none where nobody named one, echoed `engine_default`, like a designed
/// voice; realtime refused it `voice_not_configured` (live run 3, 4d). A
/// voice someone names is sent; the owner's `default_voice` only when the
/// model has it (N2).
#[test]
fn an_engine_with_a_voice_of_its_own_is_sent_none() {
    let mut f = facts(None);
    let none = RealtimeSettings::default();
    let owner = RealtimeSettings {
        default_voice: "alba".into(),
        ..Default::default()
    };
    for unvoiced in [
        Unvoiced::EngineDefault { voice: None },
        Unvoiced::EngineDefault {
            voice: Some("af_heart"),
        },
    ] {
        f.unvoiced = unvoiced;
        for s in [&none, &owner] {
            let Decision::Outcome(VoiceOutcome::Resolved(v)) = decide("t", &name("marin"), &f, s)
            else {
                panic!("the engine's own voice");
            };
            assert_eq!(
                (v.send, v.name.as_str(), v.via, v.verified),
                (None, ENGINE_DEFAULT, VoiceVia::EngineDefault, true),
                "{unvoiced:?}"
            );
        }
    }
    // Named voices are sent: the client's, a known default, a preset.
    assert_eq!(
        resolved(decide("t", &name("me_11s"), &f, &owner))
            .0
            .as_deref(),
        Some("me_11s")
    );
    let library = RealtimeSettings {
        default_voice: "me_11s".into(),
        ..Default::default()
    };
    assert_eq!(
        resolved(decide("t", &name("marin"), &f, &library)).2,
        VoiceVia::DefaultVoice
    );
    let mut preset = f.clone();
    preset.default_preset = Some(Value::String("narrator".into()));
    assert_eq!(
        resolved(decide("t", &name("marin"), &preset, &none)).2,
        VoiceVia::DefaultPreset
    );
}

/// R5 F1 (live run 3c): OmniVoice, named no voice, draws a new speaker per
/// request, and the session's seed does not hold it across clause texts —
/// one answer's clauses spoke at 222, 118 and 222 Hz. It is no longer
/// `engine_default`: with nothing configured, or a `default_voice` it does
/// not know, the session's voice is missing, and the message says why. A
/// voice it knows — the client's, a library clip or preset as the default,
/// the row's default preset — is sent as before.
#[test]
fn an_engine_that_draws_its_speaker_needs_a_voice() {
    let mut f = facts(None);
    f.unvoiced = Unvoiced::DrawsSpeaker;
    let set = |v: &str| RealtimeSettings {
        default_voice: v.into(),
        ..Default::default()
    };
    for configured in ["", "alba"] {
        let Decision::Outcome(VoiceOutcome::Missing(why)) =
            decide("t", &name("marin"), &f, &set(configured))
        else {
            panic!("{configured:?}: no voice, no answer");
        };
        assert!(
            why.contains("draws a new speaker for every request")
                && why.contains("configure a voice clip or preset"),
            "{why}"
        );
        assert_eq!(
            why.contains("realtime.default_voice ('alba') is not a voice it is known to have"),
            !configured.is_empty(),
            "{why}"
        );
        let o = resolve(Some("t"), &name("marin"), &f, &set(configured)).unwrap();
        assert_eq!(o.name(), None, "echoed null");
    }
    // Its list, read, without the default: the same.
    let mut listed = f.clone();
    listed.listed = Some(vec!["me_11s".into()]);
    assert!(matches!(
        decide("t", &name("marin"), &listed, &set("alba")),
        Decision::Outcome(VoiceOutcome::Missing(_))
    ));
    // A voice it knows speaks: a library clip or a preset as the default,
    // the client's own, the row's default preset.
    for known in ["me_11s", "narrator"] {
        assert_eq!(
            resolved(decide("t", &name("marin"), &f, &set(known))),
            (Some(known.into()), known.into(), VoiceVia::DefaultVoice)
        );
    }
    assert_eq!(
        resolved(decide("t", &name("me_11s"), &f, &set(""))).2,
        VoiceVia::Model
    );
    let mut preset = f.clone();
    preset.default_preset = Some(Value::String("narrator".into()));
    assert_eq!(
        resolved(decide("t", &name("marin"), &preset, &set("alba"))).2,
        VoiceVia::DefaultPreset
    );
    preset.default_preset = Some(serde_json::json!({"voice_ref": "/models/voices/anna.wav"}));
    assert_eq!(
        resolved(decide("t", &name("marin"), &preset, &set(""))),
        (None, "anna".into(), VoiceVia::DefaultPreset)
    );
}

/// R3 N4: an engine that clones from reference audio (CosyVoice3) refuses
/// an inline default preset without a clip — said before the response
/// starts (`voice_not_configured`), not by the engine's 500.
#[test]
fn a_cloning_engine_needs_a_clip_in_its_inline_preset() {
    let mut f = facts(None);
    f.unvoiced = Unvoiced::NeedsReference;
    let s = RealtimeSettings::default();
    f.default_preset = Some(serde_json::json!({"voice_id": "anna"}));
    let Decision::Outcome(VoiceOutcome::Missing(why)) = decide("t", &name("marin"), &f, &s) else {
        panic!("no clip, no voice");
    };
    assert!(why.contains("reference audio"), "{why}");
    f.default_preset = Some(serde_json::json!({"voice_ref": "/models/voices/anna.wav"}));
    assert_eq!(
        resolved(decide("t", &name("marin"), &f, &s)),
        (None, "anna".into(), VoiceVia::DefaultPreset)
    );
    // Nothing configured: missing, as for any model.
    f.default_preset = None;
    assert!(matches!(
        decide("t", &name("marin"), &f, &s),
        Decision::Outcome(VoiceOutcome::Missing(_))
    ));
}

/// R4 D5 (live run 3b, 6): a TTS that clones from reference audio takes the
/// owner's `default_voice` only when it is one of its clips — a library
/// clip or a preset with a `voice_ref`. Another model's voice (Pocket's
/// `alba`) was sent provisionally: the response started, the container came
/// up, and only then was it refused. Skipped, it is missing before anything
/// starts — unless the row's default preset loads a clip.
#[test]
fn a_cloning_engine_takes_only_a_clip_as_the_default_voice() {
    let mut f = facts(None);
    f.unvoiced = Unvoiced::NeedsReference;
    let set = |v: &str| RealtimeSettings {
        default_voice: v.into(),
        ..Default::default()
    };
    // Another model's voice, and a preset without a clip: skipped.
    for other in ["alba", "narrator"] {
        let Decision::Outcome(VoiceOutcome::Missing(why)) =
            decide("t", &name("marin"), &f, &set(other))
        else {
            panic!("{other}: no clip, no voice");
        };
        assert!(
            why.contains("realtime.default_voice") && why.contains(other),
            "{why}"
        );
    }
    // A library clip, or a preset that loads one: sent.
    assert_eq!(
        resolved(decide("t", &name("marin"), &f, &set("me_11s"))),
        (
            Some("me_11s".into()),
            "me_11s".into(),
            VoiceVia::DefaultVoice
        )
    );
    f.clip_presets = vec!["narrator".into()];
    assert_eq!(
        resolved(decide("t", &name("marin"), &f, &set("narrator"))).2,
        VoiceVia::DefaultVoice
    );
    // Skipped, the row's default preset with a clip still speaks.
    f.default_preset = Some(serde_json::json!({"voice_ref": "/models/voices/anna.wav"}));
    assert_eq!(
        resolved(decide("t", &name("marin"), &f, &set("alba"))),
        (None, "anna".into(), VoiceVia::DefaultPreset)
    );
}

/// R4 review (R5): lmgw lists the voice library only when it is the
/// class's `voice_dir`; a voice dir mounted for the engine alone
/// (`extra_run_args`, a supported setup) leaves `library` unread. D5 then
/// passed over a `default_voice` that is a clip there — `anna.wav` — so
/// every audio response was refused, the message saying it is no clip.
/// While lmgw cannot see the engine's voices, a TTS that needs one named
/// is sent the default provisionally, as any model is, and the first
/// clause's list decides; a name the list shows is one of its clips.
///
/// R5 review (R6): the session forgets that list before the next response
/// so its first clause reads it afresh, and with it the default went out
/// provisionally again — admitted, then refused, on every response. The
/// list seen still decides. And no `voice_dir` at all is no blind spot:
/// the engine answers to no clip, and lmgw knows it.
#[test]
fn a_default_voice_lmgw_cannot_see_is_sent_provisionally() {
    let set = |v: &str| RealtimeSettings {
        default_voice: v.into(),
        ..Default::default()
    };
    for (unvoiced, not) in [
        (
            Unvoiced::NeedsReference,
            "not in its voice list, nor a preset of its row with a voice_ref",
        ),
        (
            Unvoiced::DrawsSpeaker,
            "not a preset of its row, nor in its voice list",
        ),
    ] {
        let mut f = facts(None);
        f.library = None;
        f.unvoiced = unvoiced;
        let Decision::Outcome(VoiceOutcome::Resolved(v)) =
            decide("t", &name("marin"), &f, &set("anna"))
        else {
            panic!("{unvoiced:?}: provisional");
        };
        assert_eq!(
            (v.send.as_deref(), v.via, v.verified),
            (Some("anna"), VoiceVia::DefaultVoice, false),
            "{unvoiced:?}"
        );
        // The list, read, shows it: verified.
        f.listed = Some(vec!["anna".into()]);
        let Decision::Outcome(VoiceOutcome::Resolved(v)) =
            decide("t", &name("marin"), &f, &set("anna"))
        else {
            panic!("{unvoiced:?}: listed");
        };
        assert_eq!((v.send.as_deref(), v.verified), (Some("anna"), true));
        // The list lacks it: as the session reads it (`voices_read`, list
        // and seen), passed over — missing, and the message names only what
        // lmgw looked at.
        f.listed = Some(vec!["bob".into()]);
        f.seen = f.listed.clone();
        let Decision::Outcome(VoiceOutcome::Missing(why)) =
            decide("t", &name("marin"), &f, &set("anna"))
        else {
            panic!("{unvoiced:?}: not listed");
        };
        assert!(why.contains("('anna')") && why.contains(not), "{why}");
        // R6: what the next response starts with — the list forgotten for a
        // fresh read (`refresh_voice`), seen kept — still decides: missing
        // before anything starts, not provisional again.
        f.listed = None;
        let Decision::Outcome(VoiceOutcome::Missing(again)) =
            decide("t", &name("marin"), &f, &set("anna"))
        else {
            panic!("{unvoiced:?}: forgotten, still seen");
        };
        assert_eq!(again, why);
        // A name the list seen shows is taken, unverified until the fresh
        // read.
        f.seen = Some(vec!["anna".into()]);
        let Decision::Outcome(VoiceOutcome::Resolved(v)) =
            decide("t", &name("marin"), &f, &set("anna"))
        else {
            panic!("{unvoiced:?}: seen");
        };
        assert_eq!((v.send.as_deref(), v.verified), (Some("anna"), false));
    }
    // R6: with no voice_dir the engine answers to no clip, and lmgw knows
    // it (`library` empty, not unread): a default it is not known to have
    // is passed over before anything starts, the message saying why the
    // library offers none.
    for unvoiced in [Unvoiced::NeedsReference, Unvoiced::DrawsSpeaker] {
        let mut f = facts(None);
        f.library = Some(Vec::new());
        f.unvoiced = unvoiced;
        let Decision::Outcome(VoiceOutcome::Missing(why)) =
            decide("t", &name("marin"), &f, &set("alba"))
        else {
            panic!("{unvoiced:?}: no voice_dir");
        };
        assert!(
            why.contains("('alba')") && why.contains("voice_dir is empty"),
            "{why}"
        );
        let id = Voice::Id { id: "alba".into() };
        let Decision::NotFound(why) = decide("t", &id, &f, &set("")) else {
            panic!("{unvoiced:?}: no clip by id");
        };
        assert!(why.contains("answers to no voice-library clip"), "{why}");
    }
    // A preset of a cloning row that loads no clip is never one, seen or
    // not, and the message says so.
    let mut f = facts(None);
    f.library = None;
    f.unvoiced = Unvoiced::NeedsReference;
    let Decision::Outcome(VoiceOutcome::Missing(why)) =
        decide("t", &name("marin"), &f, &set("narrator"))
    else {
        panic!("a preset without a clip");
    };
    assert!(why.contains("a preset of its row that loads none"), "{why}");
    // The library seen: a name only the list shows is a clip as well.
    let mut f = facts(Some(&["anna"]));
    f.unvoiced = Unvoiced::NeedsReference;
    assert_eq!(
        resolved(decide("t", &name("marin"), &f, &set("anna"))),
        (Some("anna".into()), "anna".into(), VoiceVia::DefaultVoice)
    );
}
