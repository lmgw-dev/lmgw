//! Personality profiles (personality-profiles design §3.4):
//! `lmgw__profiles`, `lmgw__profile_set`, `lmgw__profile_delete`. Their
//! dispatch is `selfadmin/chat_profiles.rs`.

use crate::mcp::selfadmin::{enum_p, int_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__profiles",
            writes: false,
            description:
                "List the Chat's personality profiles: how a Chat thread's model talks, in \
                 text and in voice. Each has its id, name, persona, length_rule, examples \
                 (as text: 'User: …' then 'Reply: …' lines, a blank line between \
                 exchanges), voice_block (null = the generic one, '' = none), reasoning \
                 (null = inherit), tts_alias, voice, speech_style, and used_by (the threads \
                 and folders that picked it, which a delete would clear). default_profile_id \
                 is the one new threads start with. 'Default' is no profile, not a row.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__profile_set",
            writes: true,
            description:
                "Create or change a personality profile. Create takes a name and any of the \
                 other fields. Update takes the profile's id and only the fields to change; \
                 an empty or absent field stays as it is, and 'clear' resets one. A \
                 non-empty persona takes the thread prompt's place; length_rule is a \
                 concrete rule (one to three sentences, no recap); examples are 3-5 short \
                 exchanges as text, e.g. 'User: Hi\\nReply: Hello.' with a blank line \
                 between exchanges and two spaces before a continuation line. Edits apply \
                 from the next turn of every thread using the profile. Picking a profile for \
                 a thread is a Chat setting, not a tool.",
            props: vec![
                (
                    "action",
                    enum_p("'create' needs name; 'update' needs id.", &["create", "update"]),
                ),
                ("id", int_p("The profile to change (update).")),
                (
                    "name",
                    str_p("Unique in any case; 'default' is reserved. Required on create."),
                ),
                (
                    "persona",
                    str_p("Who the model is and how it sounds. Replaces the thread prompt."),
                ),
                (
                    "length_rule",
                    str_p("How long replies are, as a concrete rule."),
                ),
                (
                    "examples",
                    str_p(
                        "Static example exchanges in the text form: 'User: …' line, 'Reply: …' \
                         line, a blank line between exchanges.",
                    ),
                ),
                (
                    "voice_block",
                    str_p("The voice turn's own block, with voice_block_mode 'own' (the default when this is given)."),
                ),
                (
                    "voice_block_mode",
                    enum_p(
                        "'generic' = the Chat's generic voice block, 'none' = no voice block, \
                         'own' = the text in voice_block.",
                        &["generic", "none", "own"],
                    ),
                ),
                (
                    "reasoning",
                    enum_p(
                        "Reasoning for this profile's threads; 'inherit' leaves it to the \
                         thread and the route.",
                        &["inherit", "on", "off"],
                    ),
                ),
                (
                    "tts_alias",
                    str_p("Text-to-speech alias (task tts or vdes) for spoken replies."),
                ),
                ("voice", str_p("A voice of that text-to-speech model.")),
                (
                    "speech_style",
                    str_p(
                        "The speaking style it is given, with speech_style_mode 'own' (the \
                         default when this is given).",
                    ),
                ),
                (
                    "speech_style_mode",
                    enum_p(
                        "'inherit' = the thread's, then the Chat's speech style (unset), \
                         'none' = no speech style at all, 'own' = the text in speech_style.",
                        &["inherit", "none", "own"],
                    ),
                ),
                (
                    "clear",
                    str_p(
                        "Field names to unset, comma- or space-separated: persona, \
                         length_rule, examples, tts_alias, voice, speech_style (= speech_style_mode \
                         'inherit'). (For the voice block and reasoning use voice_block_mode and \
                         reasoning; for no speech style at all, speech_style_mode 'none'.)",
                    ),
                ),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__profile_delete",
            writes: true,
            description:
                "Delete a personality profile, for good. Every thread using it goes back to \
                 no profile, the folder defaults naming it lose it, and the Chat's default \
                 for new threads is emptied if it was this one. The result names the \
                 threads and folders cleared; lmgw__profiles shows beforehand what that \
                 will be (used_by).",
            props: vec![("id", int_p("The profile to delete."))],
            required: &["id"],
        },
    ]
}
