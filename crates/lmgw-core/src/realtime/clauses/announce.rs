//! What the clause splitter reports besides clauses: where a block the voice
//! skips begins ([`Block`]), and what a Chat voice says there ([`Announce`],
//! chat-voice design §6.2).
//!
//! Every speaking response skips fenced code and tables (`blocks`). A stock
//! `/v1/realtime` session skips them silently: its transcript is what was
//! said. The Chat's callers — read-aloud and a realtime session bound to a
//! thread — pass an [`Announce`] to the speech splitter instead
//! (`responder::speech::Splitter`), which says one short clause where a
//! skipped block starts: "Code block." or "Code block, rust." (a fence with
//! an info string), and "Table.". The words come from the small table
//! below, keyed by the voice's language hint, English when it has none or
//! one the table lacks. There is no public knob: announcing is the Chat's
//! own behaviour.

use super::Placed;

/// A block the voice skips, reported where it begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// A fenced code block; `info` is its info string's first word
    /// ("rust" for ```` ```rust ````), when it has one.
    Code { info: Option<String> },
    /// A table: its first row, a line that starts with `|`.
    Table,
}

/// One thing the splitter hands on, in stream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Clause(Placed),
    /// A skipped block begins here, after the clauses before it: at this
    /// byte offset into everything pushed (as [`Placed::start`] is).
    Block(Block, usize),
}

/// The words for one language: what a code block and a table are called.
struct Words {
    lang: &'static str,
    code: &'static str,
    table: &'static str,
}

/// The table the announcements come from (module doc). English first: it is
/// the fallback.
const WORDS: [Words; 5] = [
    Words {
        lang: "en",
        code: "Code block",
        table: "Table",
    },
    Words {
        lang: "de",
        code: "Codeblock",
        table: "Tabelle",
    },
    Words {
        lang: "fr",
        code: "Bloc de code",
        table: "Tableau",
    },
    Words {
        lang: "es",
        code: "Bloque de código",
        table: "Tabla",
    },
    Words {
        lang: "it",
        code: "Blocco di codice",
        table: "Tabella",
    },
];

/// What a Chat voice says where a skipped block begins (module doc).
#[derive(Clone, Copy)]
pub struct Announce {
    words: &'static Words,
}

impl std::fmt::Debug for Announce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Announce({})", self.words.lang)
    }
}

impl Announce {
    /// The announcements in `language` (an ISO 639-1 hint, the thread's
    /// reply language), English for none or one the table lacks.
    pub fn for_language(language: Option<&str>) -> Self {
        let lang = language.map(|l| l.trim().to_ascii_lowercase());
        let words = WORDS
            .iter()
            .find(|w| lang.as_deref() == Some(w.lang))
            .unwrap_or(&WORDS[0]);
        Self { words }
    }

    /// The language the words are in.
    pub fn language(&self) -> &'static str {
        self.words.lang
    }

    /// The clause said where `block` begins.
    pub fn say(&self, block: &Block) -> String {
        match block {
            Block::Code { info: Some(info) } => {
                format!("{}, {}.", self.words.code, speakable_info(info))
            }
            Block::Code { info: None } => format!("{}.", self.words.code),
            Block::Table => format!("{}.", self.words.table),
        }
    }
}

/// A fence's info word as a voice can say it (WP4 review n4): the
/// announcement goes to the TTS as it is, past the speakable pass, so the
/// symbols of `c++`, `c#` or `objective-c` are words here.
fn speakable_info(info: &str) -> String {
    info.replace("++", " plus plus")
        .replace('#', " sharp")
        .replace(['-', '_', '.', '/'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_words_follow_the_language_with_english_as_the_fallback() {
        let code = |info: Option<&str>| Block::Code {
            info: info.map(str::to_string),
        };
        let en = Announce::for_language(None);
        assert_eq!(en.say(&code(None)), "Code block.");
        assert_eq!(en.say(&code(Some("rust"))), "Code block, rust.");
        assert_eq!(en.say(&Block::Table), "Table.");
        let de = Announce::for_language(Some("de"));
        assert_eq!(de.say(&code(None)), "Codeblock.");
        assert_eq!(de.say(&code(Some("python"))), "Codeblock, python.");
        assert_eq!(de.say(&Block::Table), "Tabelle.");
        assert_eq!(Announce::for_language(Some("DE ")).language(), "de");
        // A language the table lacks speaks English.
        assert_eq!(Announce::for_language(Some("ja")).language(), "en");
        // An info word's symbols are said as words.
        assert_eq!(en.say(&code(Some("c++"))), "Code block, c plus plus.");
        assert_eq!(en.say(&code(Some("c#"))), "Code block, c sharp.");
        assert_eq!(
            en.say(&code(Some("objective-c"))),
            "Code block, objective c."
        );
    }
}
