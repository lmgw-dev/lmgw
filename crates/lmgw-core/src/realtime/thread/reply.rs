//! What a spoken reply's finalize writes (chat-voice design §8.3, §8.4):
//! the stored reply, what was heard of it, and whether it carries a tool
//! record decide.
//!
//! **What was heard** comes from the core ([`Heard`]): the whole reply, or
//! the text its heard table says was heard *as the model wrote it*
//! (`HeardTable::written`) — never the transcript, which carries the
//! announcements' words ("Codeblock, rust."). So the heard part is a prefix
//! of the stored reply, which is every delta the turn streamed, and the
//! unheard rest is the stored reply with that prefix removed.
//!
//! **A clause said otherwise than written** (WP8 review M1). The heard part
//! ends with the clause the cut fell in *as it was said*, and the speakable
//! pass rewrites a clause before it is said: markdown stripped, "z. B."
//! spelled out, parentheses turned into commas, whitespace collapsed, a tag
//! made canonical. A cut inside such a clause then is no prefix of the
//! reply, and German voice replies are full of them. So [`Heard::Part`]
//! also carries `whole` (`HeardTable::written_whole`): the clauses heard
//! whole and what was written before the partial one — always the model's
//! own text. The decision tries `exact`, then `whole` (the partial clause
//! goes to `unheard` whole), and only a reply that starts with neither is
//! left uncut: [`Write::Unmatched`], and the caller says so. When not even
//! one clause was heard whole, `whole` is empty, and the nobody-heard rule
//! below applies.
//!
//! | Case | Write |
//! |---|---|
//! | heard whole (or the heard part is all of it) | [`Write::Annotate`] |
//! | heard in part | [`Write::Cut`]: `content` the heard part, the rest `unheard` |
//! | heard none, no tool record | [`Write::Delete`] |
//! | heard none, or only of the preamble, with a tool record | [`Write::Cut`]: `content` the record's own text, the final answer `unheard` — the calls happened, and the preamble counts as heard whole |

/// What the listener heard of a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Heard {
    /// Everything: the response played out, nothing was cut away.
    Whole,
    /// Part of it (module doc): `exact`, what the model wrote up to the cut
    /// with the clause it fell in as far as it was said; `whole`, the same
    /// without that clause. Both empty: nothing.
    Part { exact: String, whole: String },
}

impl Heard {
    /// Nothing at all was heard.
    pub(crate) fn nothing() -> Self {
        Self::Part {
            exact: String::new(),
            whole: String::new(),
        }
    }
}

/// What the finalize writes (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Write {
    Annotate,
    Cut { content: String, unheard: String },
    Delete,
    Unmatched,
}

/// Decide for a stored reply `content` whose tool record holds `record`'s
/// text (`None`: no tool record), heard as `heard` (module doc).
pub(crate) fn decide(content: &str, record: Option<&str>, heard: &Heard) -> Write {
    let Heard::Part { exact, whole } = heard else {
        return Write::Annotate;
    };
    match decide_part(content, record, exact) {
        // Said otherwise than written: the words of the partial clause
        // that were written as said, then what was heard whole (module doc).
        Write::Unmatched if whole.trim() != exact.trim() => {
            let kept = written_as_said(content.trim_start(), exact.trim(), whole.trim());
            decide_part(content, record, kept.unwrap_or(whole))
        }
        write => write,
    }
}

/// `exact` trimmed back until it is the start of `body` and ends at a word
/// there — its trailing punctuation first ("Wert," said for "Wert ("),
/// then one word at a time — never down to `whole` or shorter (WP11
/// binding review NIT 5). "Das ist **wichtig**." cut after "wichtig" was
/// said "Das ist wichtig": "Das ist" was heard, and is byte for byte the
/// model's own text. `None` when nothing longer than `whole` matches.
fn written_as_said<'a>(body: &str, exact: &'a str, whole: &str) -> Option<&'a str> {
    let mut kept = exact;
    loop {
        let bare = kept.trim_end_matches(|c: char| !c.is_alphanumeric());
        kept = if bare.len() < kept.len() {
            bare
        } else {
            kept[..kept.rfind(char::is_whitespace)?].trim_end()
        };
        if kept.len() <= whole.len() {
            return None;
        }
        let ends_a_word = |rest: &str| rest.chars().next().is_none_or(|c| !c.is_alphanumeric());
        if body.strip_prefix(kept).is_some_and(ends_a_word) {
            return Some(kept);
        }
    }
}

/// [`decide`] for a reply of which `heard` was heard.
fn decide_part(content: &str, record: Option<&str>, heard: &str) -> Write {
    let heard = heard.trim();
    let body = content.trim_start();
    let cut = |at: usize| {
        let rest = body[at..].trim();
        if rest.is_empty() {
            Write::Annotate
        } else {
            Write::Cut {
                content: body[..at].trim_end().to_string(),
                unheard: rest.to_string(),
            }
        }
    };
    match record {
        None if heard.is_empty() => Write::Delete,
        None => match body.starts_with(heard) {
            true => cut(heard.len()),
            false => Write::Unmatched,
        },
        Some(record) => {
            // The record's text leads the reply; what follows it is the
            // final answer (`chat_turn::final_answer`).
            let Some(answer_at) = content.strip_prefix(record).map(|_| record.len()) else {
                // A record that does not lead the reply: cut only where the
                // heard text is the reply's own start, and never delete.
                return match !heard.is_empty() && body.starts_with(heard) {
                    true => cut(heard.len()),
                    false => Write::Unmatched,
                };
            };
            // Both trimmed at the start alike: the record is the reply's
            // own beginning.
            if heard.len() > record.trim().len() && body.starts_with(heard) {
                // Cut inside the final answer: only that.
                return cut(heard.len());
            }
            if !heard.is_empty() && !body.starts_with(heard) {
                return Write::Unmatched;
            }
            // Cut in the preamble, or while a tool ran: the record whole.
            let answer = content[answer_at..].trim();
            if answer.is_empty() {
                Write::Annotate
            } else {
                Write::Cut {
                    content: record.to_string(),
                    unheard: answer.to_string(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Heard up to `s`, said as written.
    fn part(s: &str) -> Heard {
        Heard::Part {
            exact: s.into(),
            whole: s.into(),
        }
    }

    #[test]
    fn a_plain_reply_is_annotated_cut_or_deleted() {
        let reply = "Hallo Jürgen, schön dich zu hören.";
        assert_eq!(decide(reply, None, &Heard::Whole), Write::Annotate);
        assert_eq!(decide(reply, None, &part(reply)), Write::Annotate);
        assert_eq!(
            decide(reply, None, &part("Hallo Jürgen,")),
            Write::Cut {
                content: "Hallo Jürgen,".into(),
                unheard: "schön dich zu hören.".into()
            }
        );
        assert_eq!(decide(reply, None, &part("")), Write::Delete);
        assert_eq!(decide(reply, None, &part("  ")), Write::Delete);
        // A cut inside a clause, by character.
        assert_eq!(
            decide(reply, None, &part("Hallo Jürgen, sch")),
            Write::Cut {
                content: "Hallo Jürgen, sch".into(),
                unheard: "ön dich zu hören.".into()
            }
        );
        // Said otherwise than written: left uncut.
        assert_eq!(
            decide("**Hallo** du", None, &part("Hallo")),
            Write::Unmatched
        );
        // The reply's own leading whitespace does not stand in the way.
        assert_eq!(
            decide("\nEins. Zwei.", None, &part("Eins.")),
            Write::Cut {
                content: "Eins.".into(),
                unheard: "Zwei.".into()
            }
        );
    }

    #[test]
    fn a_clause_said_otherwise_falls_back_to_what_was_heard_whole() {
        let said = |exact: &str, whole: &str| Heard::Part {
            exact: exact.into(),
            whole: whole.into(),
        };
        let reply = "Hallo. Das ist z. B. so. Und weiter.";
        // The cut fell inside the clause said "Das ist zum Beispiel so.":
        // the words of it written as said were heard (WP11 binding review
        // NIT 5).
        assert_eq!(
            decide(reply, None, &said("Hallo. Das ist zum Bei", "Hallo.")),
            Write::Cut {
                content: "Hallo. Das ist".into(),
                unheard: "z. B. so. Und weiter.".into()
            }
        );
        // Its trailing punctuation was the speakable pass's, the word the
        // model's.
        assert_eq!(
            decide(
                "Ja. Der Wert (etwa zehn) passt.",
                None,
                &said("Ja. Der Wert,", "Ja.")
            ),
            Write::Cut {
                content: "Ja. Der Wert".into(),
                unheard: "(etwa zehn) passt.".into()
            }
        );
        // A word cut short in the reply is no word heard: "Wel" of "Welt".
        assert_eq!(
            decide(
                "Hallo. Weltmeister **sind** wir.",
                None,
                &said("Hallo. Welt sind", "Hallo.")
            ),
            Write::Cut {
                content: "Hallo.".into(),
                unheard: "Weltmeister **sind** wir.".into()
            }
        );
        // Every word of the clause said otherwise: what was heard whole.
        assert_eq!(
            decide(reply, None, &said("Hallo. Zum Bei", "Hallo.")),
            Write::Cut {
                content: "Hallo.".into(),
                unheard: "Das ist z. B. so. Und weiter.".into()
            }
        );
        // Cut before the clause parts from what was written: the exact cut.
        assert_eq!(
            decide(reply, None, &said("Hallo. Das", "Hallo.")),
            Write::Cut {
                content: "Hallo. Das".into(),
                unheard: "ist z. B. so. Und weiter.".into()
            },
        );
        // Inside the first clause, said otherwise: nothing heard whole, and
        // nobody heard it.
        assert_eq!(decide("*Ha*, ja.", None, &said("Ha,", "")), Write::Delete);
        // With a tool record: a cut inside the final answer cuts only that,
        // at the words written as said…
        let record = "Ich schaue nach. ";
        let tooled = "Ich schaue nach. Es sind z. B. 21 Grad.";
        assert_eq!(
            decide(
                tooled,
                Some(record),
                &said("Ich schaue nach. Es sind zum", record)
            ),
            Write::Cut {
                content: "Ich schaue nach. Es sind".into(),
                unheard: "z. B. 21 Grad.".into()
            }
        );
        // …and none of them: the record stays whole, the answer unheard.
        assert_eq!(
            decide(tooled, Some(record), &said("Ich schaue nach. Esse", record)),
            Write::Cut {
                content: record.into(),
                unheard: "Es sind z. B. 21 Grad.".into()
            }
        );
        // Neither is the reply's start: left uncut.
        assert_eq!(
            decide("**Hallo** du. Ja.", None, &said("Hallo du. J", "Hallo du.")),
            Write::Unmatched
        );
    }

    #[test]
    fn an_announced_block_is_the_history_s_only_when_heard() {
        // The heard table's `written()` for a cut inside "Codeblock, rust."
        // and for one exactly after it (`heard::written`'s tests).
        let reply = "Hier:\n```rust\nfn main() {}\n```\nFertig.";
        assert_eq!(
            decide(reply, None, &part("Hier:")),
            Write::Cut {
                content: "Hier:".into(),
                unheard: "```rust\nfn main() {}\n```\nFertig.".into()
            }
        );
        assert_eq!(
            decide(reply, None, &part("Hier:\n```rust\nfn main() {}\n```")),
            Write::Cut {
                content: "Hier:\n```rust\nfn main() {}\n```".into(),
                unheard: "Fertig.".into()
            }
        );
    }

    #[test]
    fn a_reply_with_a_tool_record_is_never_deleted() {
        // The preamble "Ich schaue nach. " is the record's text; the final
        // answer follows it.
        let record = "Ich schaue nach. ";
        let reply = "Ich schaue nach. Es sind 21 Grad.";
        // Nothing heard: the record stays whole, the answer is unheard.
        assert_eq!(
            decide(reply, Some(record), &part("")),
            Write::Cut {
                content: record.into(),
                unheard: "Es sind 21 Grad.".into()
            }
        );
        // Part of the preamble: the preamble counts as heard whole.
        assert_eq!(
            decide(reply, Some(record), &part("Ich schaue")),
            Write::Cut {
                content: record.into(),
                unheard: "Es sind 21 Grad.".into()
            }
        );
        // Inside the final answer: only that is cut.
        assert_eq!(
            decide(reply, Some(record), &part("Ich schaue nach. Es sind")),
            Write::Cut {
                content: "Ich schaue nach. Es sind".into(),
                unheard: "21 Grad.".into()
            }
        );
        assert_eq!(decide(reply, Some(record), &Heard::Whole), Write::Annotate);
        // No preamble: a tool ran before anything was said.
        assert_eq!(
            decide("Es sind 21 Grad.", Some(""), &part("")),
            Write::Cut {
                content: String::new(),
                unheard: "Es sind 21 Grad.".into()
            }
        );
        // A record and nothing after it: there is no answer to move.
        assert_eq!(decide(record, Some(record), &part("")), Write::Annotate);
    }
}
