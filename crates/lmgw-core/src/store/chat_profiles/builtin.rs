//! The built-in profiles (personality-profiles design §1.3, D11). Their
//! texts are not stored: a built-in row follows them for every field its
//! body leaves absent, so it keeps up as they improve, the way
//! `BUILTIN_CHAT_SYSTEM_PROMPT` does. Migration 0071 seeds each once; a
//! deleted one is created again only on request.

use crate::config::chat_profile::{BuiltinProfile, Reasoning, BUILTIN_CONCISE};

/// "Concise": a voice assistant that answers in a few sentences. Written
/// for the model; `{{model}}` and `{{date}}` are filled in as in the thread
/// prompt. The voice block is the generic one (with its language sentence
/// and date rule), and reasoning is off, since thinking is the costliest
/// delay before a spoken reply.
pub const CONCISE: BuiltinProfile = BuiltinProfile {
    key: BUILTIN_CONCISE,
    name: "Concise",
    persona: "You are a voice assistant on the owner's own machine, reached through lmgw, a \
self-hosted LLM gateway. You are the model behind the alias \"{{model}}\". Today is {{date}}. \
You talk like a capable colleague: direct, friendly, in plain words. When you are not sure, say \
so briefly. You can call tools only when tool definitions come with the request; without them, \
never claim to have looked something up.",
    length_rule: "Answer in one to three sentences. Give details only when asked for them. Do \
not end with a summary or a recap, and do not offer further help (no \"let me know if…\").",
    examples: &[
        (
            "Should I take an umbrella today?",
            "I can't see the weather from here, so I don't know. A quick look at a forecast \
will tell you.",
        ),
        (
            "What's the capital of Australia?",
            "Canberra. Many people guess Sydney, but Canberra was built as a compromise between \
Sydney and Melbourne.",
        ),
        (
            "How does a heat pump work?",
            "It moves heat instead of making it. A refrigerant picks up warmth from the outside \
air, a compressor makes it hotter, and it gives that heat off indoors.",
        ),
    ],
    voice_block: None,
    reasoning: Some(Reasoning::Off),
};

/// Every built-in profile.
pub const BUILTINS: &[BuiltinProfile] = &[CONCISE];

/// The built-in with `key`, when this build has one.
pub fn builtin(key: &str) -> Option<&'static BuiltinProfile> {
    BUILTINS.iter().find(|b| b.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CHAT_PROMPT_DATE, CHAT_PROMPT_MODEL};

    #[test]
    fn the_seeded_row_names_a_known_builtin() {
        let sql = include_str!("../../../migrations/0071_chat_profiles.sql");
        assert!(
            sql.contains(&format!("('{}', '{}', '{{}}')", CONCISE.name, CONCISE.key)),
            "0071 seeds Concise by its key"
        );
        assert_eq!(builtin(BUILTIN_CONCISE).unwrap().name, "Concise");
        assert!(builtin("chatty").is_none());
    }

    #[test]
    fn the_persona_uses_both_placeholders_and_the_texts_are_trimmed() {
        assert!(CONCISE.persona.contains(CHAT_PROMPT_MODEL));
        assert!(CONCISE.persona.contains(CHAT_PROMPT_DATE));
        for t in [CONCISE.persona, CONCISE.length_rule]
            .into_iter()
            .chain(CONCISE.examples.iter().flat_map(|(u, r)| [*u, *r]))
        {
            assert_eq!(t, t.trim());
            assert!(!t.is_empty());
            assert!(
                !t.contains("  "),
                "a line break in the source left a gap: {t}"
            );
        }
    }
}
