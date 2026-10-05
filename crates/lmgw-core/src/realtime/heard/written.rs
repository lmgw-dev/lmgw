//! What the model wrote, up to what was heard (realtime design §7.2, §7.3;
//! B2 review 2).
//!
//! The spoken transcript is what the voice said: fenced code, markdown and
//! the like are left out of it (§8.1). The model's history must not lose
//! them — "run the second command again" needs the commands — so each
//! clause carries what the model wrote for it, and the history is that text
//! up to the heard cut:
//! - a clause heard whole gives what the model wrote for it, and before it
//!   the text the voice left out since the clause before;
//! - the clause the cut falls in gives the text left out before it, and
//!   then the part of it that was heard, as it was said, to the last word
//!   heard whole (`words.rs`) — the model must not believe its words after
//!   the cut were heard;
//! - text after the last clause (a code block that ends the answer) is part
//!   of it, unless audio was cut away; text left unsaid before a tool call
//!   with clauses after it stays where it was written, before them (B4
//!   review) — it used to be emitted after them.
//!
//! What was written before a clause nobody heard a word of goes with that
//! clause: a code block between the last heard clause and the cut-off one
//! is not part of the history.
//!
//! **An announcement is no text the model wrote** (chat-voice design §6.2,
//! §8.4; WP4 review M2). The Chat's voice says "Table." or "Code block,
//! rust." where it skips a block ([`Written::announcement`]). Its words are
//! the transcript's only — never the history's, whole or in part — and the
//! block counts as heard when its announcement was heard whole:
//! - an announcement carries what was written before its block (`before`),
//!   and the clause after it carries the block;
//! - heard whole, with the cut before the next clause: the block is the
//!   history's — the announcement keeps it (`keep`) — and so is a trailing
//!   block, as the tail;
//! - cut inside it: what was written before the block is the history's, the
//!   words said and the block are not.

use super::HeardTable;

/// What the model wrote for one clause.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Written {
    /// Text between the clause before and this one that was not said:
    /// fenced code, list markers, rule lines, a clause with nothing to say,
    /// the whitespace between.
    pub before: String,
    /// The clause as the model wrote it, markdown and abbreviations as they
    /// were. Empty for an announcement, until a cut right after it gives it
    /// the block it announced (module doc).
    pub own: String,
    /// The clause is an announcement of a skipped block (module doc): what
    /// it says is not the model's.
    pub announcement: bool,
}

impl Written {
    /// A clause written exactly as it was said, `before` its separator.
    pub fn said(before: &str, text: &str) -> Self {
        Self {
            before: before.to_string(),
            own: text.to_string(),
            announcement: false,
        }
    }

    /// An announcement (module doc), after `before`: the text left out
    /// since the clause before, up to the block it announces.
    pub fn announcing(before: &str) -> Self {
        Self {
            before: before.to_string(),
            own: String::new(),
            announcement: true,
        }
    }
}

impl HeardTable {
    /// What the model wrote, up to what was heard (module doc): the item's
    /// text in the model's history.
    pub fn written(&self) -> String {
        let mut out = String::new();
        for c in &self.clauses {
            out.push_str(&c.written.before);
            out.push_str(match (c.partial, c.written.announcement) {
                (false, _) => &c.written.own,
                (true, false) => &c.text,
                // Cut inside an announcement: its words were the voice's,
                // and its block was not heard (module doc).
                (true, true) => "",
            });
        }
        if !self.clipped {
            out.push_str(&self.tail);
        }
        out.trim().to_string()
    }

    /// [`Self::written`] without the heard part of the clause the cut fell
    /// in: the clauses heard whole, as the model wrote them, and what it
    /// wrote before that clause. Always the model's own text, where
    /// `written()` ends with the partial clause as it was *said* — which is
    /// not what the model wrote when the speakable pass changed it (markdown
    /// stripped, "z. B." spelled out, parentheses turned into commas, a tag
    /// made canonical; chat-voice design §8.4). The history falls back to it
    /// then, and the whole clause goes to `unheard`.
    pub fn written_whole(&self) -> String {
        let mut out = String::new();
        for c in &self.clauses {
            out.push_str(&c.written.before);
            if c.partial {
                // The partial clause is always the last (`keep`).
                return out.trim().to_string();
            }
            out.push_str(&c.written.own);
        }
        if !self.clipped {
            out.push_str(&self.tail);
        }
        out.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Hier der Befehl:" for 1 s, a code block not said, then "Führ ihn
    /// aus." for 1 s, and a closing code block — 24 kHz.
    fn answer() -> HeardTable {
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_written(
            "Hier der Befehl:",
            Written::said("", "**Hier** der Befehl:\n"),
            24_000,
        );
        t.push_written(
            "Führ ihn aus.",
            Written::said("```sh\nls -la\n```\n", "Führ ihn aus.\n"),
            24_000,
        );
        t.push_unspoken("```sh\nrm -rf build\n```");
        t
    }

    #[test]
    fn heard_whole_is_everything_the_model_wrote() {
        let t = answer();
        assert_eq!(
            t.written(),
            "**Hier** der Befehl:\n```sh\nls -la\n```\nFühr ihn aus.\n```sh\nrm -rf build\n```"
        );
        assert_eq!(t.text(), "Hier der Befehl: Führ ihn aus.", "what was said");
        let mut kept = answer();
        kept.keep(u64::MAX);
        assert_eq!(kept.written(), t.written(), "nothing was cut away");
    }

    #[test]
    fn a_cut_keeps_what_came_before_the_clause_it_falls_in() {
        // Half of "Führ ihn aus." heard (6 characters, "Führ i"): the code
        // before it is the model's, the clause only to the last word heard
        // whole, and the tail is not.
        let mut t = answer();
        t.truncate(1500).unwrap();
        assert_eq!(
            t.written(),
            "**Hier** der Befehl:\n```sh\nls -la\n```\nFühr"
        );
        assert_eq!(t.text(), "Hier der Befehl: Führ");
        // Inside its first word: nobody heard a word of it, and the code
        // before it goes with it.
        let mut t = answer();
        t.truncate(1200).unwrap();
        assert_eq!(t.written(), "**Hier** der Befehl:");
        assert_eq!(t.text(), "Hier der Befehl:");
    }

    #[test]
    fn a_cut_at_a_clause_end_leaves_out_what_the_next_one_brought() {
        let mut t = answer();
        t.truncate(1000).unwrap();
        assert_eq!(t.written(), "**Hier** der Befehl:");
        let mut nothing = answer();
        nothing.keep(0);
        assert_eq!(nothing.written(), "");
    }

    #[test]
    fn text_left_unsaid_before_more_clauses_stays_where_it_was_written() {
        // A tool call mid-answer: what the model wrote before it and did not
        // say reaches the table before the clauses after the call.
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_written("Moment.", Written::said("", "Moment.\n"), 24_000);
        t.push_unspoken("```\nquery()\n```\n");
        t.push_written("Hier ist es.", Written::said("", "Hier ist es."), 24_000);
        assert_eq!(t.written(), "Moment.\n```\nquery()\n```\nHier ist es.");
        // Cut before the second clause: the unsaid text went with it.
        t.keep(24_000);
        assert_eq!(t.written(), "Moment.");
    }

    /// "Vergleich:" for 1 s, the announcement "Tabelle." for 1 s, then
    /// "Fertig." for 1 s — the table written between them, as the Chat's
    /// speech splitter hands it on (`responder::speech`).
    fn announced() -> HeardTable {
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_written("Vergleich:", Written::said("", "Vergleich:\n"), 24_000);
        t.push_written("Tabelle.", Written::announcing(""), 24_000);
        t.push_written(
            "Fertig.",
            Written::said("| a | b |\n|---|---|\n", "Fertig."),
            24_000,
        );
        t
    }

    #[test]
    fn an_announcement_is_never_the_history_s() {
        // Heard whole: everything the model wrote, and nothing it did not.
        let t = announced();
        assert_eq!(t.written(), "Vergleich:\n| a | b |\n|---|---|\nFertig.");
        assert_eq!(t.text(), "Vergleich: Tabelle. Fertig.", "what was said");
        // Cut inside it: neither its words nor the table it announced.
        let mut t = announced();
        assert_eq!(t.truncate(1500).unwrap(), "Vergleich:");
        assert_eq!(t.written(), "Vergleich:");
        let mut t = announced();
        t.keep(30_000);
        assert_eq!(t.written(), "Vergleich:");
    }

    #[test]
    fn a_block_is_heard_when_its_announcement_was_heard_whole() {
        // Cut exactly after it: the table is the history's, "Fertig." not.
        let mut t = announced();
        t.truncate(2000).unwrap();
        assert_eq!(t.written(), "Vergleich:\n| a | b |\n|---|---|");
        // Cut inside the clause after it: the table, and the words of it
        // heard whole — none inside "Fertig."'s only word.
        let mut t = announced();
        t.truncate(2500).unwrap();
        assert_eq!(t.written(), "Vergleich:\n| a | b |\n|---|---|");
        let mut t = announced();
        t.push_written("Und mehr.", Written::said(" ", "Und mehr."), 24_000);
        t.truncate(3800).unwrap();
        assert_eq!(t.written(), "Vergleich:\n| a | b |\n|---|---|\nFertig. Und");
        // A second cut inside the announcement takes the table back out.
        let mut t = announced();
        t.truncate(2000).unwrap();
        t.truncate(1500).unwrap();
        assert_eq!(t.written(), "Vergleich:");
        // A trailing block reaches the history as the tail, heard or not.
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_written("Hier:", Written::said("", "Hier:\n"), 24_000);
        t.push_written("Codeblock.", Written::announcing(""), 24_000);
        t.push_unspoken("```\nls\n```");
        assert_eq!(t.written(), "Hier:\n```\nls\n```");
        t.truncate(1500).unwrap();
        assert_eq!(t.written(), "Hier:");
    }

    #[test]
    fn of_two_blocks_in_a_row_only_the_one_announced_whole_is_heard() {
        // "Hier:", then a code block and a table, each announced; the
        // announcement of the table carries the code before it.
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_written("Hier:", Written::said("", "Hier:\n"), 24_000);
        t.push_written("Codeblock.", Written::announcing(""), 24_000);
        t.push_written("Tabelle.", Written::announcing("```\nls\n```\n"), 24_000);
        t.push_written("Ende.", Written::said("| a |\n", "Ende."), 24_000);
        let whole = t.clone();
        assert_eq!(whole.written(), "Hier:\n```\nls\n```\n| a |\nEnde.");
        let mut code = t.clone();
        code.truncate(2000).unwrap();
        assert_eq!(code.written(), "Hier:\n```\nls\n```");
        let mut half = t.clone();
        half.truncate(2500).unwrap();
        assert_eq!(
            half.written(),
            "Hier:\n```\nls\n```",
            "the table was not heard"
        );
        t.truncate(3000).unwrap();
        assert_eq!(t.written(), "Hier:\n```\nls\n```\n| a |");
    }

    #[test]
    fn written_whole_stops_before_the_clause_the_cut_fell_in() {
        // "z. B." said as "zum Beispiel": the partial clause as said is no
        // prefix of what the model wrote; the clauses before it are.
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_written("Hallo.", Written::said("", "Hallo. "), 24_000);
        t.push_written(
            "Das ist zum Beispiel so.",
            Written::said("```\nls\n```\n", "Das ist z. B. so. "),
            24_000,
        );
        t.push_written("Und weiter.", Written::said("", "Und weiter."), 24_000);
        let whole = t.clone();
        assert_eq!(whole.written_whole(), whole.written(), "nothing was cut");
        t.truncate(1500).unwrap();
        assert_eq!(t.written(), "Hallo. ```\nls\n```\nDas ist zum");
        assert_eq!(
            t.written_whole(),
            "Hallo. ```\nls\n```",
            "the code before the clause was passed, the clause was not heard whole"
        );
        // A cut at a clause end: both say the same.
        let mut t = whole.clone();
        t.truncate(1000).unwrap();
        assert_eq!(t.written_whole(), "Hallo.");
        assert_eq!(t.written(), t.written_whole());
        // Inside the first clause: nothing was heard whole.
        let mut t = whole;
        t.truncate(500).unwrap();
        assert_eq!(t.written_whole(), "");
    }

    #[test]
    fn a_client_item_is_written_as_it_was_said() {
        let mut t = HeardTable::new(24_000).unwrap();
        t.push_clause("Eins.", 2400);
        t.push_clause("Zwei.", 2400);
        assert_eq!(t.written(), "Eins. Zwei.");
        t.truncate(150).unwrap();
        assert_eq!(t.written(), "Eins.", "no word of \"Zwei.\" was heard whole");
    }
}
