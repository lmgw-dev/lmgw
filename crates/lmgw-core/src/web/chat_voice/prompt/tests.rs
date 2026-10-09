use super::*;
use crate::config::{DEFAULT_VOICE_INSTRUCTIONS, VOICE_FOLLOWS_USER, VOICE_PERSONA};
use crate::store::ChatThread;

/// [`voice_block`] for a thread whose prompt is `raw`, with no profile:
/// opened when the prompt is not empty, `realtime.default_instructions`'
/// text.
fn vblock(
    raw: &str,
    s: &RealtimeSettings,
    hint: Option<&str>,
    language: Option<&TurnLanguage>,
) -> String {
    voice_block(
        raw,
        !raw.trim().is_empty(),
        s.default_instructions.as_deref(),
        hint,
        language,
    )
}

/// `thread`'s turn with no profile.
fn turn(thread: &ChatThread) -> Prompt<'_> {
    Prompt {
        thread,
        profile: None,
        model: "m",
        today: chrono::NaiveDate::from_ymd_opt(2026, 10, 9).unwrap(),
    }
}

/// A turn whose user speaks `speaks` and whose reply is in `reply`.
fn lang(speaks: Option<&str>, reply: &str, spoken: bool) -> TurnLanguage {
    TurnLanguage {
        reply: reply.into(),
        speaks: speaks.map(str::to_string),
        spoken,
    }
}

#[test]
fn the_built_in_text_is_still_what_realtime_sends() {
    assert_eq!(
        DEFAULT_VOICE_INSTRUCTIONS,
        "You are a voice assistant. Your replies are spoken aloud, so keep them short and \
         conversational: use plain sentences, and never use markdown, lists, tables or code. \
         Answer in the language the user speaks. You do not know the current date or time \
         unless the conversation says it."
    );
    assert_eq!(
        DEFAULT_VOICE_INSTRUCTIONS,
        format!("{VOICE_PERSONA} {VOICE_STYLE} {VOICE_NO_DATE}")
    );
    assert_eq!(VOICE_STYLE, format!("{VOICE_FORM} {VOICE_FOLLOWS_USER}"));
}

#[test]
fn the_block_follows_the_thread_s_prompt_in_order() {
    let s = RealtimeSettings::default();
    let block = vblock(
        "You are an assistant.",
        &s,
        Some("[laughs] is a sound."),
        None,
    );
    assert_eq!(
        block,
        format!("{BRIDGE} {VOICE_STYLE} {VOICE_NO_DATE}\n\n[laughs] is a sound.")
    );
    assert!(!block.contains(VOICE_PERSONA));
    // A prompt that says the date keeps it; the "no date" line goes.
    let dated = vblock("Today is {{date}}.", &s, None, None);
    assert_eq!(dated, format!("{BRIDGE} {VOICE_STYLE}"));
    // No thread prompt: no bridge.
    assert_eq!(
        vblock("", &s, None, None),
        format!("{VOICE_STYLE} {VOICE_NO_DATE}")
    );
    assert_eq!(
        with_block("You are an assistant.".into(), &block),
        format!("You are an assistant.\n\n{block}")
    );
}

#[test]
fn the_owner_s_own_text_is_verbatim_and_empty_is_none() {
    let own = RealtimeSettings {
        default_instructions: Some("Sprich kurz.".into()),
        ..Default::default()
    };
    assert_eq!(
        vblock("P", &own, None, None),
        format!("{BRIDGE} Sprich kurz.")
    );
    let off = RealtimeSettings {
        default_instructions: Some(String::new()),
        ..Default::default()
    };
    assert_eq!(vblock("P", &off, None, None), "");
    assert_eq!(vblock("P", &off, Some("hint"), None), "hint");
    assert_eq!(with_block("P".into(), ""), "P");
}

#[test]
fn the_language_replaces_the_built_in_text_s_follow_the_user() {
    let s = RealtimeSettings::default();
    let german = language_sentence(Some("de"), "de", true);
    assert_eq!(
        german,
        "The user speaks German and hears your reply in a German voice, so answer in German \
         unless the user asks for another language."
    );
    let block = vblock(
        "You are an assistant.",
        &s,
        Some("hint"),
        Some(&lang(Some("de"), "de", true)),
    );
    assert_eq!(
        block,
        format!("{BRIDGE} {VOICE_FORM} {german} {VOICE_NO_DATE}\n\nhint")
    );
    assert!(!block.contains(VOICE_FOLLOWS_USER), "{block}");
    // A code lmgw has no name for is named as the code.
    assert!(language_sentence(Some("sw"), "sw", true).starts_with("The user speaks sw and"));
    // Blank is none.
    assert_eq!(
        vblock("", &s, None, Some(&lang(None, " ", true))),
        format!("{VOICE_STYLE} {VOICE_NO_DATE}")
    );
}

#[test]
fn the_language_is_its_own_paragraph_beside_the_owner_s_text_or_none() {
    let german = language_sentence(Some("de"), "de", true);
    let own = RealtimeSettings {
        default_instructions: Some("Sprich kurz.".into()),
        ..Default::default()
    };
    assert_eq!(
        vblock("P", &own, Some("hint"), Some(&lang(Some("de"), "de", true))),
        format!("{BRIDGE} Sprich kurz.\n\n{german}\n\nhint")
    );
    let off = RealtimeSettings {
        default_instructions: Some(String::new()),
        ..Default::default()
    };
    assert_eq!(
        vblock("P", &off, None, Some(&lang(Some("de"), "de", true))),
        german
    );
}

#[test]
fn a_turn_that_is_no_voice_turn_gets_the_language_alone() {
    let s = RealtimeSettings::default();
    let thread = ChatThread {
        system_prompt: "P".into(),
        ..Default::default()
    };
    let spoken = lang(Some("de"), "de", true);
    // `speak: true`: the reply is heard.
    let (block, reasoning) = turn_block(&turn(&thread), &s, None, Some(&spoken));
    assert_eq!(
        block.as_deref(),
        Some(language_sentence(Some("de"), "de", true).as_str())
    );
    assert_eq!(reasoning, None, "the thread's own reasoning");
    // A text-output session's turn: the user spoke, the reply is read.
    let read = lang(Some("de"), "de", false);
    let (block, _) = turn_block(&turn(&thread), &s, None, Some(&read));
    assert_eq!(
        block.as_deref(),
        Some("The user speaks German, so answer in German unless the user asks for another language.")
    );
    // No language, no voice: nothing, as before.
    assert_eq!(turn_block(&turn(&thread), &s, None, None).0, None);
    // A voice turn: the voice block, the language inside it.
    let voice = VoiceTurn { hint: None };
    let (block, reasoning) = turn_block(&turn(&thread), &s, Some(&voice), Some(&spoken));
    assert_eq!(
        block.unwrap(),
        vblock("P", &s, None, Some(&lang(Some("de"), "de", true)))
    );
    assert_eq!(reasoning.unwrap().enabled, Some(false));
}

#[test]
fn a_voice_turn_thinks_only_when_the_thread_says_so() {
    let s = RealtimeSettings::default();
    let thread = |enabled: Option<bool>, effort: Option<&str>| ChatThread {
        reasoning_enabled: enabled,
        reasoning_effort: effort.map(str::to_string),
        ..Default::default()
    };
    // Left at the route's default: off.
    let (_, r) = voice_request(&turn(&thread(None, None)), &s, None, None);
    assert_eq!(r.unwrap().enabled, Some(false));
    // An explicit setting wins: on, an effort alone, or off.
    let (_, r) = voice_request(&turn(&thread(Some(true), Some("high"))), &s, None, None);
    let r = r.unwrap();
    assert_eq!((r.enabled, r.effort.as_deref()), (Some(true), Some("high")));
    let (_, r) = voice_request(&turn(&thread(None, Some("low"))), &s, None, None);
    let r = r.unwrap();
    assert_eq!((r.enabled, r.effort.as_deref()), (None, Some("low")));
}

#[test]
fn two_languages_go_into_the_block_and_alone() {
    let s = RealtimeSettings::default();
    let split = lang(Some("de"), "en", true);
    let sentence = "The user speaks German and hears your reply in an English voice, so answer \
                    in English unless the user asks for another language.";
    assert_eq!(
        vblock("You are an assistant.", &s, None, Some(&split)),
        format!("{BRIDGE} {VOICE_FORM} {sentence} {VOICE_NO_DATE}")
    );
    let thread = ChatThread::default();
    let (block, _) = turn_block(
        &turn(&thread),
        &s,
        None,
        Some(&lang(Some("de"), "en", false)),
    );
    assert_eq!(
        block.as_deref(),
        Some("The user speaks German, but answer in English unless the user asks for another language.")
    );
    let (block, _) = turn_block(&turn(&thread), &s, None, Some(&lang(None, "en", true)));
    assert_eq!(
        block.as_deref(),
        Some("The user hears your reply in an English voice, so answer in English unless the user asks for another language.")
    );
}
