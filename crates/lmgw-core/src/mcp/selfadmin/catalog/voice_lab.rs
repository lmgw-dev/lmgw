//! The Voices lab on the tool plane: `lmgw__voice_generate` (a round of
//! candidate samples from a text-to-speech model) and
//! `lmgw__voice_sample_save` (one of them into the voice library). Their
//! dispatch is `selfadmin/voice_lab.rs`; both are the `voice_generate` and
//! `voice_sample_save` ops.

use crate::mcp::selfadmin::{bool_p, int_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__voice_generate",
            writes: true,
            description: "Render candidate voice samples (the dashboard's Voices lab): a fixed \
                 voice for the cloning text-to-speech models (OmniVoice, CosyVoice3, Fish, \
                 Qwen3-TTS Base), which need a reference clip with its exact transcript. Any \
                 text-to-speech alias, local or cloud, says the same text as `count` \
                 candidates (4 when absent, no cap), each with its own seed where the engine \
                 reads one. Use a voice-design model (description) or one with fixed voices \
                 (voice). The text is `text`, else the sentence for `language` (a phonetically \
                 rich passage; the owner may have saved their own). A configured fallback \
                 answers as for any request; the answer names who answered. Candidates are \
                 kept under the round's id until discarded in the Voices lab; the owner \
                 listens there. Then lmgw__voice_sample_save saves the chosen one.",
            props: vec![
                (
                    "alias",
                    str_p("The text-to-speech model, e.g. 'audio/qwen3-voicedesign'."),
                ),
                (
                    "text",
                    str_p("What every candidate says. Empty: the sentence for `language`."),
                ),
                (
                    "language",
                    str_p(
                        "ISO 639-1 code (en, de): picks the sentence and is sent to a local \
                         model that takes a language.",
                    ),
                ),
                (
                    "description",
                    str_p(
                        "The voice description (a voice-design model: 'a warm, calm female \
                         voice in her forties') or speaking style. Empty: none.",
                    ),
                ),
                (
                    "voice",
                    str_p("The voice for a model that speaks one it is told. Empty: its default."),
                ),
                (
                    "count",
                    int_p("How many candidates, rendered one after another. Default 4."),
                ),
                (
                    "seed",
                    int_p(
                        "The first candidate's seed; the next get seed+1, seed+2, … Empty: a \
                         random seed each.",
                    ),
                ),
            ],
            required: &["alias"],
        },
        Builtin {
            name: "lmgw__voice_sample_save",
            writes: true,
            description: "Save a candidate from lmgw__voice_generate to the voice library as \
                 <name>.wav: leading and trailing silence cut (loudness untouched), the text \
                 it says recorded as its transcript, how it was made recorded beside it. Every \
                 cloning text-to-speech model can then speak with it as voice=<name>, \
                 realtime sessions included. A name the library already has is refused \
                 (name_taken) and nothing is written; pass overwrite=true only when the owner \
                 confirmed replacing that sample.",
            props: vec![
                ("round", str_p("The round's id, from lmgw__voice_generate.")),
                (
                    "candidate",
                    int_p("The candidate's number in the round, from 1."),
                ),
                (
                    "name",
                    str_p(
                        "The voice name: letters, digits, space, '.', '_' and '-'. A trailing \
                         .wav is dropped.",
                    ),
                ),
                (
                    "overwrite",
                    bool_p("Replace a sample of that name. Default false."),
                ),
            ],
            required: &["round", "candidate", "name"],
        },
    ]
}
