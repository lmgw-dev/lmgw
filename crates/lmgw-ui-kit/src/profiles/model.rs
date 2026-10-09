//! The editor's form as plain data: what the boxes hold, how it becomes the
//! wire's draft, create body and patch, what changed, and the sentences the
//! editor says about counts and deletes. No signals here, so all of it is
//! tested natively.

use lmgw_api_types::chat_profiles::{
    field, Example, PreviewTokens, Profile, ProfileCreate, ProfileDraft, ProfilePatch,
    ProfileVoice, Reasoning, UsedBy, RESERVED_NAME,
};

/// What the voice block's three-way choice holds (design D6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockMode {
    /// `null`: the Chat's generic block.
    Generic,
    /// `""`: no block.
    None,
    /// A text of its own, used verbatim.
    Own,
}

/// What the speech style's three-way choice holds (§1.1: `""` is none, as
/// a thread's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StyleMode {
    /// `null`: the thread's, then the Chat's.
    #[default]
    Inherit,
    /// `""`: no speech style at all.
    None,
    /// A text of its own.
    Own,
}

/// The form as the boxes hold it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Form {
    pub name: String,
    pub persona: String,
    pub length_rule: String,
    /// Rows as typed; a row with both sides empty is not an example.
    pub examples: Vec<Example>,
    pub block_mode: Option<BlockMode>,
    pub block_text: String,
    pub reasoning: Option<Reasoning>,
    pub tts_alias: String,
    pub voice: String,
    pub style_mode: StyleMode,
    pub speech_style: String,
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

impl Form {
    /// The form of a stored profile (or of a new one: `None`).
    pub fn of(p: Option<&Profile>) -> Self {
        let Some(p) = p else {
            return Self {
                block_mode: Some(BlockMode::Generic),
                ..Self::default()
            };
        };
        let (mode, text) = match p.voice_block.as_deref() {
            None => (BlockMode::Generic, String::new()),
            Some("") => (BlockMode::None, String::new()),
            Some(t) => (BlockMode::Own, t.to_string()),
        };
        Self {
            name: p.name.clone(),
            persona: p.persona.clone(),
            length_rule: p.length_rule.clone(),
            examples: p.examples.clone(),
            block_mode: Some(mode),
            block_text: text,
            reasoning: p.reasoning,
            tts_alias: p.voice.tts_alias.clone().unwrap_or_default(),
            voice: p.voice.voice.clone().unwrap_or_default(),
            style_mode: match p.voice.speech_style.as_deref() {
                None => StyleMode::Inherit,
                Some("") => StyleMode::None,
                Some(_) => StyleMode::Own,
            },
            speech_style: p.voice.speech_style.clone().unwrap_or_default(),
        }
    }

    fn mode(&self) -> BlockMode {
        self.block_mode.unwrap_or(BlockMode::Generic)
    }

    /// The examples that are complete; empty rows are dropped, half-filled
    /// ones are [`problems`](Self::problems).
    pub fn wire_examples(&self) -> Vec<Example> {
        self.examples
            .iter()
            .filter(|e| !e.user.trim().is_empty() && !e.reply.trim().is_empty())
            .map(|e| Example {
                user: e.user.trim().to_string(),
                reply: e.reply.trim().to_string(),
            })
            .collect()
    }

    pub fn wire_block(&self) -> Option<String> {
        match self.mode() {
            BlockMode::Generic => None,
            BlockMode::None => Some(String::new()),
            BlockMode::Own => Some(self.block_text.clone()),
        }
    }

    /// An empty box inherits (`null`); the speech style as its mode says
    /// (`""` for none), so a stored "none" is written back as it was.
    pub fn wire_voice(&self) -> ProfileVoice {
        ProfileVoice {
            tts_alias: opt(&self.tts_alias),
            voice: opt(&self.voice),
            speech_style: match self.style_mode {
                StyleMode::Inherit => None,
                StyleMode::None => Some(String::new()),
                StyleMode::Own => opt(&self.speech_style),
            },
        }
    }

    /// The content fields as Preview, Test and Speak take them.
    pub fn draft(&self) -> ProfileDraft {
        ProfileDraft {
            persona: self.persona.clone(),
            length_rule: self.length_rule.clone(),
            examples: self.wire_examples(),
            voice_block: self.wire_block(),
            reasoning: self.reasoning,
            voice: self.wire_voice(),
        }
    }

    /// What blocks a save, as sentences. The server's own checks (taken
    /// names, unknown aliases) come back as its refusal.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(n) = name_problem(&self.name) {
            out.push(n);
        }
        for (i, e) in self.examples.iter().enumerate() {
            if e.user.trim().is_empty() != e.reply.trim().is_empty() {
                out.push(format!(
                    "example {}: both the user's line and the reply are needed",
                    i + 1
                ));
            }
        }
        if self.mode() == BlockMode::Own && self.block_text.trim().is_empty() {
            out.push("voice block: write it, or pick \"none\" for no block".to_string());
        }
        if self.style_mode == StyleMode::Own && self.speech_style.trim().is_empty() {
            out.push("speech style: write it, or pick \"inherit\" or \"none\"".to_string());
        }
        out
    }

    /// The create body for a new profile.
    pub fn create(&self) -> ProfileCreate {
        let d = self.draft();
        ProfileCreate {
            name: Some(self.name.trim().to_string()),
            builtin: None,
            persona: d.persona,
            length_rule: d.length_rule,
            examples: d.examples,
            voice_block: d.voice_block,
            reasoning: d.reasoning,
            voice: d.voice,
        }
    }

    /// The fields that differ from `base`, by the wire's names (`name` and
    /// the five content fields, `voice` as one).
    pub fn changed(&self, base: &Form) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.name.trim() != base.name.trim() {
            out.push("name");
        }
        if self.persona != base.persona {
            out.push(field::PERSONA);
        }
        if self.length_rule != base.length_rule {
            out.push(field::LENGTH_RULE);
        }
        if self.wire_examples() != base.wire_examples() {
            out.push(field::EXAMPLES);
        }
        if self.wire_block() != base.wire_block() {
            out.push(field::VOICE_BLOCK);
        }
        if self.reasoning != base.reasoning {
            out.push(field::REASONING);
        }
        if self.wire_voice() != base.wire_voice() {
            out.push("voice");
        }
        out
    }

    /// The patch carrying only what changed. A built-in row that is left
    /// alone keeps following the built-in text, which is why unchanged
    /// fields are absent rather than written back.
    pub fn patch(&self, base: &Form) -> ProfilePatch {
        let mut p = ProfilePatch::default();
        for f in self.changed(base) {
            match f {
                "name" => p.name = Some(self.name.trim().to_string()),
                field::PERSONA => p.persona = Some(Some(self.persona.clone())),
                field::LENGTH_RULE => p.length_rule = Some(Some(self.length_rule.clone())),
                field::EXAMPLES => p.examples = Some(Some(self.wire_examples())),
                field::VOICE_BLOCK => p.voice_block = Some(self.wire_block()),
                field::REASONING => p.reasoning = Some(self.reasoning),
                "voice" => p.voice = Some(Some(self.wire_voice())),
                _ => {}
            }
        }
        p
    }
}

/// Is the name usable, as far as the editor can tell?
pub fn name_problem(name: &str) -> Option<String> {
    let t = name.trim();
    if t.is_empty() {
        Some("name: a profile needs a name".to_string())
    } else if t.eq_ignore_ascii_case(RESERVED_NAME) {
        Some("name: \"Default\" is no profile (it is what a thread has without one)".to_string())
    } else {
        None
    }
}

/// What deleting would clear, for the armed button ("used by 2 threads and
/// the folder 'Assistant'"). Empty usage says so, so the question is still
/// asked about something.
pub fn delete_question(used: &UsedBy, is_default: bool, builtin: bool) -> String {
    let mut parts = Vec::new();
    if used.threads > 0 {
        parts.push(format!(
            "{} thread{}",
            used.threads,
            if used.threads == 1 { "" } else { "s" }
        ));
    }
    let names: Vec<String> = used
        .folders
        .iter()
        .map(|f| format!("'{}'", f.name))
        .collect();
    match names.len() {
        0 => {}
        1 => parts.push(format!("the folder {}", names[0])),
        _ => parts.push(format!("the folders {}", names.join(", "))),
    }
    if is_default {
        parts.push("the Chat setting for new threads".to_string());
    }
    let mut q = if parts.is_empty() {
        "Delete? nothing uses it".to_string()
    } else {
        format!("Delete? used by {}; they go back to none", join_and(&parts))
    };
    if builtin {
        q.push_str(" (it can be added back)");
    }
    q
}

fn join_and(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// An approximation flag in words (`x-lmgw-count-approximate`).
pub fn approx_words(flag: &str) -> String {
    match flag {
        "flattened" => "counted as plain text: the chat template and per-message overhead are not in the number",
        "tokenizer_guess" => "the model's tokenizer is unknown, a stand-in counted",
        "media_bound" => "images are counted at the model's upper bound",
        "media_omitted" => "image or audio parts are not in the number",
        "message_framing" => "the text went as one user message, its framing is in the number",
        other => return other.to_string(),
    }
    .to_string()
}

/// "412 tokens on <alias>", the alias that answered when it was another,
/// and each approximation in words.
pub fn token_line(t: &PreviewTokens, turn: Turn) -> String {
    let n = match turn {
        Turn::Static => t.static_part,
        Turn::Text => t.text_turn,
        Turn::Voice => t.voice_turn,
    };
    let mut s = format!("{} tokens on {}", crate::fmt::grouped(n), t.alias);
    if let Some(by) = &t.answered_by {
        s.push_str(&format!(" (answered by {by})"));
    }
    for a in &t.approx {
        s.push_str(&format!(" · approximate: {}", approx_words(a)));
    }
    s
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    Static,
    Text,
    Voice,
}

/// The sample Speak starts with: the last Test reply, else the first
/// example's reply (editable, so this is only the start).
pub fn speak_default(last_reply: Option<&str>, examples: &[Example]) -> String {
    last_reply
        .filter(|r| !r.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            examples
                .iter()
                .find(|e| !e.reply.trim().is_empty())
                .map(|e| e.reply.clone())
        })
        .unwrap_or_default()
}

/// Test's timing line: first token, reasoning time and who answered.
pub fn timing_line(first_ms: Option<u64>, reasoning_ms: Option<u64>, total_ms: u64) -> String {
    let mut parts = Vec::new();
    if let Some(f) = first_ms {
        parts.push(format!("first token {f} ms"));
    }
    if let Some(r) = reasoning_ms {
        parts.push(format!("reasoning {r} ms"));
    }
    parts.push(format!("total {total_ms} ms"));
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::chat_profiles::FolderRef;

    fn ex(u: &str, r: &str) -> Example {
        Example {
            user: u.into(),
            reply: r.into(),
        }
    }

    fn stored() -> Profile {
        Profile {
            id: 4,
            name: "Brief".into(),
            persona: "Calm.".into(),
            examples: vec![ex("hi", "hello")],
            voice_block: Some(String::new()),
            reasoning: Some(Reasoning::Off),
            voice: ProfileVoice {
                tts_alias: Some("tts".into()),
                voice: None,
                speech_style: Some("warm".into()),
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_stored_profile_round_trips_through_the_form() {
        let p = stored();
        let f = Form::of(Some(&p));
        assert_eq!(f.block_mode, Some(BlockMode::None));
        let d = f.draft();
        assert_eq!(d.voice_block, Some(String::new()));
        assert_eq!(d.voice, p.voice);
        assert_eq!(d.examples, p.examples);
        assert!(f.changed(&f.clone()).is_empty());
        assert_eq!(f.patch(&f.clone()), ProfilePatch::default());
    }

    #[test]
    fn the_voice_block_has_three_states() {
        let mut f = Form::of(None);
        assert_eq!(f.wire_block(), None);
        f.block_mode = Some(BlockMode::None);
        assert_eq!(f.wire_block(), Some(String::new()));
        f.block_mode = Some(BlockMode::Own);
        f.block_text = "Short.".into();
        assert_eq!(f.wire_block(), Some("Short.".into()));
        f.block_text = " ".into();
        assert_eq!(f.problems().len(), 2); // no name, empty own block
    }

    #[test]
    fn a_patch_carries_only_what_changed() {
        let base = Form::of(Some(&stored()));
        let mut f = base.clone();
        f.persona = "Brisk.".into();
        f.reasoning = None;
        f.style_mode = StyleMode::Inherit;
        let p = f.patch(&base);
        assert_eq!(p.persona, Some(Some("Brisk.".into())));
        assert_eq!(p.reasoning, Some(None));
        assert_eq!(
            p.voice,
            Some(Some(ProfileVoice {
                tts_alias: Some("tts".into()),
                voice: None,
                speech_style: None
            }))
        );
        assert!(p.length_rule.is_none() && p.examples.is_none() && p.voice_block.is_none());
        assert!(p.name.is_none());
        assert_eq!(f.changed(&base), vec!["persona", "reasoning", "voice"]);
    }

    #[test]
    fn the_speech_style_has_three_states_and_a_stored_none_stays() {
        let mut p = stored();
        p.voice.speech_style = Some(String::new());
        let base = Form::of(Some(&p));
        assert_eq!(base.style_mode, StyleMode::None);
        assert_eq!(base.wire_voice().speech_style, Some(String::new()));
        // A voice edit beside it writes the stored "none" back as it was.
        let mut f = base.clone();
        f.voice = "alba".into();
        let patch = f.patch(&base);
        assert_eq!(
            patch.voice.unwrap().unwrap().speech_style,
            Some(String::new())
        );
        // Inherit is null; own is the text, and an empty own is a problem.
        f.style_mode = StyleMode::Inherit;
        assert_eq!(f.wire_voice().speech_style, None);
        f.style_mode = StyleMode::Own;
        f.speech_style = " calm ".into();
        assert_eq!(f.wire_voice().speech_style, Some("calm".into()));
        assert!(f.problems().is_empty());
        f.speech_style = " ".into();
        assert_eq!(f.problems().len(), 1, "{:?}", f.problems());
        // A stored text is "own"; none stored is "inherit".
        assert_eq!(Form::of(Some(&stored())).style_mode, StyleMode::Own);
        assert_eq!(Form::of(None).style_mode, StyleMode::Inherit);
    }

    #[test]
    fn examples_drop_empty_rows_and_flag_half_filled_ones() {
        let mut f = Form::of(Some(&stored()));
        f.examples.push(ex("", ""));
        assert!(f.problems().is_empty());
        assert_eq!(f.wire_examples().len(), 1);
        f.examples.push(ex("only a question", " "));
        assert_eq!(f.problems().len(), 1);
        assert!(f.problems()[0].contains("example 3"));
    }

    #[test]
    fn names_refuse_empty_and_default_in_any_case() {
        assert!(name_problem("  ").is_some());
        assert!(name_problem("DeFault").is_some());
        assert!(name_problem("Defaults").is_none());
    }

    #[test]
    fn create_trims_the_name_and_sends_the_draft() {
        let mut f = Form::of(None);
        f.name = "  Dry  ".into();
        f.persona = "Dry.".into();
        let c = f.create();
        assert_eq!(c.name.as_deref(), Some("Dry"));
        assert_eq!(c.builtin, None);
        assert_eq!(c.persona, "Dry.");
        assert_eq!(c.voice_block, None);
    }

    #[test]
    fn the_delete_question_says_what_it_clears() {
        let used = UsedBy {
            threads: 2,
            folders: vec![FolderRef {
                id: 1,
                name: "Assistant".into(),
            }],
        };
        let q = delete_question(&used, false, false);
        assert_eq!(
            q,
            "Delete? used by 2 threads and the folder 'Assistant'; they go back to none"
        );
        assert!(delete_question(&UsedBy::default(), false, false).contains("nothing uses it"));
        let q = delete_question(
            &UsedBy {
                threads: 1,
                folders: vec![],
            },
            true,
            true,
        );
        assert!(q.contains("1 thread and the Chat setting"), "{q}");
        assert!(q.ends_with("(it can be added back)"));
    }

    #[test]
    fn the_token_line_names_the_alias_and_each_approximation() {
        let t = PreviewTokens {
            alias: "local".into(),
            answered_by: Some("cloud".into()),
            static_part: 1412,
            approx: vec!["flattened".into(), "weird".into()],
            ..Default::default()
        };
        let s = token_line(&t, Turn::Static);
        assert!(
            s.starts_with(&format!(
                "{} tokens on local (answered by cloud)",
                crate::fmt::grouped(1412)
            )),
            "{s}"
        );
        assert!(s.contains("plain text"));
        assert!(s.contains("approximate: weird"));
    }

    #[test]
    fn speak_starts_from_the_last_reply_else_the_first_example() {
        let exs = vec![ex("a", ""), ex("b", "second")];
        assert_eq!(speak_default(Some("last"), &exs), "last");
        assert_eq!(speak_default(Some(" "), &exs), "second");
        assert_eq!(speak_default(None, &[]), "");
    }
}
