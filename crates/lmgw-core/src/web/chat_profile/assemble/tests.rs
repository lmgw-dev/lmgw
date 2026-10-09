use super::*;
use crate::config::{VOICE_FORM, VOICE_NO_DATE, VOICE_STYLE};

/// `chat_voice::prompt::BRIDGE`, which is private to the voice module.
const BRIDGE: &str = "This reply is spoken aloud. For it, the instructions below replace any \
                      guidance above about formatting, Markdown, tables and code.";

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 9).unwrap()
}

fn thread(prompt: &str) -> ChatThread {
    ChatThread {
        system_prompt: prompt.into(),
        model_alias: "m".into(),
        kind: "chat".into(),
        ..Default::default()
    }
}

fn prompt<'a>(t: &'a ChatThread, p: Option<Profile<'a>>) -> Prompt<'a> {
    Prompt {
        thread: t,
        profile: p,
        model: "qwen",
        today: day(),
    }
}

fn ex(user: &str, reply: &str) -> Example {
    Example {
        user: user.into(),
        reply: reply.into(),
    }
}

#[test]
fn no_profile_is_the_thread_s_prompt_expanded() {
    let t = thread("  You are {{model}}. Today is {{date}}.  ");
    assert_eq!(
        prompt(&t, None).static_part(),
        "You are qwen. Today is Friday, 9 October 2026."
    );
    assert_eq!(prompt(&thread(""), None).static_part(), "");
    // A profile with nothing set is no profile, as far as the text goes.
    assert_eq!(
        prompt(&t, Some(Profile::default())).static_part(),
        prompt(&t, None).static_part()
    );
}

#[test]
fn a_persona_takes_the_prompt_s_place_and_the_rest_follows() {
    let t = thread("The explainer.");
    let examples = [
        ex("Hi?", "Hello."),
        ex("Capital of {{model}}land?", "Two\nlines."),
    ];
    let p = Profile {
        persona: " I am {{model}}. ",
        length_rule: " One sentence. ",
        examples: &examples,
        ..Default::default()
    };
    assert_eq!(
        prompt(&t, Some(p)).static_part(),
        "I am qwen.\n\nOne sentence.\n\nExamples of how you answer:\nUser: Hi?\nYou: \
         Hello.\n\nUser: Capital of qwenland?\nYou: Two\nlines."
    );
    // No persona: the thread's prompt stays the base.
    let p = Profile {
        length_rule: "One sentence.",
        ..Default::default()
    };
    assert_eq!(
        prompt(&t, Some(p)).static_part(),
        "The explainer.\n\nOne sentence."
    );
    assert_eq!(prompt(&t, Some(p)).base(), "The explainer.");
    // A blank persona is none.
    let p = Profile {
        persona: "  ",
        ..Default::default()
    };
    assert_eq!(prompt(&t, Some(p)).static_part(), "The explainer.");
    // Examples alone, over an empty prompt.
    let one = [ex("a", "b")];
    let p = Profile {
        examples: &one,
        ..Default::default()
    };
    assert_eq!(
        prompt(&thread(""), Some(p)).static_part(),
        format!("{EXAMPLES_HEADING}\nUser: a\nYou: b")
    );
}

#[test]
fn the_voice_block_follows_the_profile_then_the_setting() {
    let t = thread("P");
    let generic = RealtimeSettings::default();
    let owner = RealtimeSettings {
        default_instructions: Some("Sprich kurz.".into()),
        ..Default::default()
    };
    let voice = VoiceTurn { hint: None };
    let sys = |p: Option<Profile>, s: &RealtimeSettings| {
        system_message(&prompt(&t, p), s, Some(&voice), None).0
    };
    // Unset: the setting's rule, as without a profile.
    let unset = Profile {
        length_rule: "Short.",
        ..Default::default()
    };
    assert_eq!(
        sys(Some(unset), &generic),
        format!("P\n\nShort.\n\n{BRIDGE} {VOICE_STYLE} {VOICE_NO_DATE}")
    );
    assert_eq!(
        sys(Some(unset), &owner),
        format!("P\n\nShort.\n\n{BRIDGE} Sprich kurz.")
    );
    // A text: verbatim, placeholders filled, over the owner's setting.
    let own = Profile {
        voice_block: Some("Talk like {{model}}."),
        ..Default::default()
    };
    assert_eq!(
        sys(Some(own), &owner),
        format!("P\n\n{BRIDGE} Talk like qwen.")
    );
    // "": none, and no bridge.
    let none = Profile {
        voice_block: Some(""),
        ..Default::default()
    };
    assert_eq!(sys(Some(none), &generic), "P");
}

#[test]
fn the_date_sentence_is_judged_on_the_persona() {
    let generic = RealtimeSettings::default();
    let voice = VoiceTurn { hint: None };
    // The thread's prompt says the date, the persona does not: the line
    // stays.
    let t = thread("Today is {{date}}.");
    let p = Profile {
        persona: "I am a voice.",
        ..Default::default()
    };
    let (sys, _) = system_message(&prompt(&t, Some(p)), &generic, Some(&voice), None);
    assert_eq!(
        sys,
        format!("I am a voice.\n\n{BRIDGE} {VOICE_STYLE} {VOICE_NO_DATE}")
    );
    // The persona says it: the line goes.
    let t = thread("No date here.");
    let p = Profile {
        persona: "Today is {{date}}.",
        ..Default::default()
    };
    let (sys, _) = system_message(&prompt(&t, Some(p)), &generic, Some(&voice), None);
    assert_eq!(
        sys,
        format!("Today is Friday, 9 October 2026.\n\n{BRIDGE} {VOICE_STYLE}")
    );
}

#[test]
fn a_language_goes_inside_the_generic_block_or_after_the_profile_s() {
    let generic = RealtimeSettings::default();
    let voice = VoiceTurn {
        hint: Some("hint".into()),
    };
    let de = TurnLanguage {
        reply: "de".into(),
        speaks: Some("de".into()),
        spoken: true,
    };
    let german = "The user speaks German and hears your reply in a German voice, so answer in \
                  German unless the user asks for another language.";
    let t = thread("P");
    let own = Profile {
        voice_block: Some("Kurz."),
        ..Default::default()
    };
    let (sys, _) = system_message(&prompt(&t, Some(own)), &generic, Some(&voice), Some(&de));
    assert_eq!(sys, format!("P\n\n{BRIDGE} Kurz.\n\n{german}\n\nhint"));
    let none = Profile {
        voice_block: Some(""),
        ..Default::default()
    };
    let (sys, _) = system_message(&prompt(&t, Some(none)), &generic, Some(&voice), Some(&de));
    assert_eq!(sys, format!("P\n\n{german}\n\nhint"));
    let (sys, _) = system_message(
        &prompt(&t, Some(Profile::default())),
        &generic,
        Some(&voice),
        Some(&de),
    );
    assert_eq!(
        sys,
        format!("P\n\n{BRIDGE} {VOICE_FORM} {german} {VOICE_NO_DATE}\n\nhint")
    );
    // A text turn with a language: the sentence after the static part.
    let p = Profile {
        persona: "I.",
        ..Default::default()
    };
    let (sys, _) = system_message(&prompt(&t, Some(p)), &generic, None, Some(&de));
    assert_eq!(sys, format!("I.\n\n{german}"));
}

#[test]
fn reasoning_goes_thread_then_profile_then_voice_off() {
    let generic = RealtimeSettings::default();
    let voice = VoiceTurn::default();
    let on = Profile {
        reasoning: Some(Reasoning::On),
        ..Default::default()
    };
    let t = thread("P");
    let enabled = |p: Option<Profile>, t: &ChatThread, v: Option<&VoiceTurn>| {
        system_message(&prompt(t, p), &generic, v, None)
            .1
            .map(|c| (c.enabled, c.effort))
    };
    // Text: the route's default, the profile's.
    assert_eq!(enabled(None, &t, None), None);
    assert_eq!(enabled(Some(on), &t, None), Some((Some(true), None)));
    // Voice: off by default, the profile's on over it.
    assert_eq!(enabled(None, &t, Some(&voice)), Some((Some(false), None)));
    assert_eq!(
        enabled(Some(on), &t, Some(&voice)),
        Some((Some(true), None))
    );
    // The thread's own field wins over both.
    let low = ChatThread {
        reasoning_effort: Some("low".into()),
        ..thread("P")
    };
    let off = Profile {
        reasoning: Some(Reasoning::Off),
        ..Default::default()
    };
    for v in [None, Some(&voice)] {
        assert_eq!(
            enabled(Some(off), &low, v),
            Some((None, Some("low".into())))
        );
    }
}

#[test]
fn an_admin_thread_wraps_what_the_profile_gives() {
    let t = ChatThread {
        kind: ADMIN_KIND.into(),
        ..thread("Own.")
    };
    let p = Profile {
        persona: "Persona.",
        length_rule: "Short.",
        ..Default::default()
    };
    let (sys, _) = system_message(
        &prompt(&t, Some(p)),
        &RealtimeSettings::default(),
        None,
        None,
    );
    assert_eq!(sys, agentchat::system_prompt("Persona.\n\nShort."));
    let (sys, _) = system_message(&prompt(&t, None), &RealtimeSettings::default(), None, None);
    assert_eq!(sys, agentchat::system_prompt("Own."));
}

#[test]
fn a_snapshot_row_that_is_gone_is_no_profile() {
    let mut snap = Snapshot::default();
    snap.chat_profiles.push(ChatProfile {
        id: 7,
        persona: "Seven.".into(),
        ..Default::default()
    });
    let mut t = thread("Own.");
    t.profile_id = Some(7);
    assert_eq!(Prompt::of(&snap, &t, "m", day()).static_part(), "Seven.");
    t.profile_id = Some(8);
    assert_eq!(Prompt::of(&snap, &t, "m", day()).static_part(), "Own.");
}
