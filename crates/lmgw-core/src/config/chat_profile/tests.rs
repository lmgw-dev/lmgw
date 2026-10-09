use super::*;

const DEF: BuiltinProfile = BuiltinProfile {
    key: "test",
    name: "Test",
    persona: "Be brief.",
    length_rule: "One sentence.",
    examples: &[("Hi?", "Hello.")],
    voice_block: None,
    reasoning: Some(Reasoning::Off),
};

fn resolve(body: ProfileBody, def: Option<&BuiltinProfile>) -> ChatProfile {
    ChatProfile::resolve(
        1,
        "P".into(),
        def.map(|d| d.key.to_string()),
        body,
        def,
        String::new(),
        String::new(),
    )
}

#[test]
fn a_builtin_row_follows_every_absent_field() {
    let p = resolve(ProfileBody::default(), Some(&DEF));
    assert_eq!(p.persona, "Be brief.");
    assert_eq!(p.length_rule, "One sentence.");
    assert_eq!(p.examples.len(), 1);
    assert_eq!(p.voice_block, None);
    assert_eq!(p.reasoning, Some(Reasoning::Off));
    assert_eq!(p.follows_builtin, field::FOLLOWABLE.map(String::from));
    // The owner's own row: absent is unset, and nothing follows.
    let p = resolve(ProfileBody::default(), None);
    assert_eq!(p.persona, "");
    assert_eq!(p.reasoning, None);
    assert!(p.follows_builtin.is_empty());
}

#[test]
fn an_explicit_null_unsets_a_builtin_field() {
    let body = ProfileBody {
        persona: Slot::Unset,
        reasoning: Slot::Unset,
        ..Default::default()
    };
    let p = resolve(body, Some(&DEF));
    assert_eq!(p.persona, "");
    assert_eq!(p.reasoning, None);
    assert_eq!(
        p.follows_builtin,
        [field::LENGTH_RULE, field::EXAMPLES, field::VOICE_BLOCK]
    );
}

#[test]
fn writing_the_builtin_text_follows_the_builtin_again() {
    let mut body = ProfileBody {
        persona: Slot::Set("Other.".into()),
        reasoning: Slot::Set(Reasoning::On),
        ..Default::default()
    };
    let mut p = ProfilePatch {
        persona: Some(Some("  Be brief. ".into())),
        reasoning: Some(Some(Reasoning::Off)),
        // The built-in's own: the generic block.
        voice_block: Some(None),
        ..Default::default()
    };
    normalise_patch(&mut p).unwrap();
    body.apply_patch(&p, Some(&DEF));
    assert_eq!(body, ProfileBody::default());
    // A different text is stored; null on a built-in row is unset.
    let p = ProfilePatch {
        length_rule: Some(None),
        voice_block: Some(Some(String::new())),
        ..Default::default()
    };
    body.apply_patch(&p, Some(&DEF));
    assert_eq!(body.length_rule, Slot::Unset);
    assert_eq!(body.voice_block, Slot::Set(String::new()));
    // On the owner's own row unset is stored as absent.
    let mut own = ProfileBody {
        persona: Slot::Set("x".into()),
        ..Default::default()
    };
    own.apply_patch(
        &ProfilePatch {
            persona: Some(Some(String::new())),
            ..Default::default()
        },
        None,
    );
    assert_eq!(own.persona, Slot::Absent);
}

#[test]
fn a_create_of_the_builtin_texts_stores_nothing() {
    let d = ProfileDraft {
        persona: "Be brief.".into(),
        length_rule: "One sentence.".into(),
        examples: vec![Example {
            user: "Hi?".into(),
            reply: "Hello.".into(),
        }],
        voice_block: None,
        reasoning: Some(Reasoning::Off),
        voice: ProfileVoice::default(),
    };
    assert_eq!(ProfileBody::from_draft(&d, Some(&DEF)).to_stored(), "{}");
    let own = ProfileBody::from_draft(&d, None);
    assert_eq!(own.persona, Slot::Set("Be brief.".into()));
    assert_eq!(own.voice_block, Slot::Absent);
}

#[test]
fn the_body_round_trips_and_reads_tolerantly() {
    let body = ProfileBody {
        persona: Slot::Set("P".into()),
        length_rule: Slot::Unset,
        examples: Slot::Set(vec![Example {
            user: "u".into(),
            reply: "r".into(),
        }]),
        voice_block: Slot::Set(String::new()),
        reasoning: Slot::Set(Reasoning::On),
        voice: ProfileVoice {
            tts_alias: Some("tts".into()),
            voice: None,
            speech_style: Some(String::new()),
        },
    };
    let text = body.to_stored();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["length_rule"], Value::Null);
    assert_eq!(
        v["voice"],
        serde_json::json!({"tts_alias": "tts", "speech_style": ""})
    );
    assert_eq!(ProfileBody::from_stored(&text), body);
    // A key this build cannot read goes alone; a newer build's key is
    // ignored; text that is no object is an empty body.
    let read = ProfileBody::from_stored(
        r#"{"persona": "P", "reasoning": "sometimes", "examples": [{"user": 1}],
            "mood": "calm", "voice": {"voice": "alba", "pitch": 3}}"#,
    );
    assert_eq!(read.persona, Slot::Set("P".into()));
    assert_eq!(read.reasoning, Slot::Absent);
    assert_eq!(read.examples, Slot::Absent);
    assert_eq!(read.voice.voice.as_deref(), Some("alba"));
    assert_eq!(ProfileBody::from_stored("[1]"), ProfileBody::default());
    assert_eq!(ProfileBody::from_stored("not json"), ProfileBody::default());
}

#[test]
fn names_are_trimmed_and_default_is_reserved_in_any_case() {
    assert_eq!(normalise_name("  Calm ").unwrap(), "Calm");
    assert_eq!(normalise_name("DeFault"), Err(ProfileRefusal::NameReserved));
    assert!(matches!(
        normalise_name("  "),
        Err(ProfileRefusal::Invalid(_))
    ));
    assert_eq!(name_key("Ärger"), name_key("äRGER"));
    assert_eq!(ProfileRefusal::NameReserved.status(), 400);
    assert_eq!(ProfileRefusal::NameTaken("x".into()).status(), 409);
    assert_eq!(
        ProfileRefusal::NameTaken("x".into()).code(),
        "profile_name_taken"
    );
}

#[test]
fn a_draft_is_trimmed_and_an_empty_example_side_refused() {
    let mut d = ProfileDraft {
        persona: " p ".into(),
        voice_block: Some("   ".into()),
        voice: ProfileVoice {
            tts_alias: Some("  ".into()),
            voice: Some(" alba ".into()),
            speech_style: Some("  ".into()),
        },
        examples: vec![Example {
            user: " u ".into(),
            reply: " r ".into(),
        }],
        ..Default::default()
    };
    normalise_draft(&mut d).unwrap();
    assert_eq!(d.persona, "p");
    assert_eq!(d.voice_block.as_deref(), Some(""));
    assert_eq!(d.voice.tts_alias, None);
    assert_eq!(d.voice.voice.as_deref(), Some("alba"));
    assert_eq!(d.voice.speech_style.as_deref(), Some(""));
    assert_eq!(d.examples[0].user, "u");
    d.examples.push(Example {
        user: "x".into(),
        reply: " ".into(),
    });
    let e = normalise_draft(&mut d).unwrap_err();
    assert!(e.to_string().contains("examples[1].reply"), "{e}");
}

#[test]
fn a_create_is_a_name_with_fields_or_a_builtin_key_alone() {
    let named = ProfileCreate {
        name: Some(" Calm ".into()),
        persona: "Be calm.".into(),
        ..Default::default()
    };
    match create_kind(&named).unwrap() {
        CreateKind::Named(n, d) => {
            assert_eq!(n, "Calm");
            assert_eq!(d.persona, "Be calm.");
        }
        other => panic!("{other:?}"),
    }
    let builtin = ProfileCreate {
        builtin: Some("concise".into()),
        ..Default::default()
    };
    assert_eq!(
        create_kind(&builtin).unwrap(),
        CreateKind::Builtin("concise".into())
    );
    let both = ProfileCreate {
        builtin: Some("concise".into()),
        persona: "x".into(),
        ..Default::default()
    };
    assert!(matches!(
        create_kind(&both),
        Err(ProfileRefusal::Invalid(_))
    ));
    assert!(matches!(
        create_kind(&ProfileCreate::default()),
        Err(ProfileRefusal::Invalid(_))
    ));
}

#[test]
fn the_snapshot_finds_a_profile_by_id_and_by_name_in_any_case() {
    let snap = Snapshot {
        chat_profiles: vec![ChatProfile {
            id: 4,
            name: "Ruhig".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(snap.chat_profile(4).unwrap().name, "Ruhig");
    assert!(snap.chat_profile(5).is_none());
    assert_eq!(snap.chat_profile_named(" RUHIG ").unwrap().id, 4);
}
