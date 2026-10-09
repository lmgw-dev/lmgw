//! The examples' text form (personality-profiles design §1.1), for the
//! self-admin tools and a paste into the editor:
//!
//! ```text
//! User: What's the capital of Australia?
//! Reply: Canberra. Many people guess Sydney,
//!   but Canberra was built as a compromise.
//!
//! User: …
//! Reply: …
//! ```
//!
//! - one exchange is a `User:` line followed by a `Reply:` line;
//! - continuation lines are indented by two spaces (a line of the text that
//!   is empty is printed as the two spaces alone);
//! - exchanges are separated by a blank line.
//!
//! Reading is lenient where a paste loses whitespace: the keywords take any
//! case, a continuation may be indented by a tab or one space, and a blank
//! line followed by a continuation is an empty line of the text (an editor
//! that strips trailing spaces leaves exactly that). Each side is trimmed
//! and must not be empty.

use super::Example;

/// `examples` in the text form. Printing and then reading gives the same
/// list back for any examples whose sides are trimmed and non-empty, which
/// is what the gateway stores.
pub fn examples_to_text(examples: &[Example]) -> String {
    examples
        .iter()
        .map(|e| {
            format!(
                "User: {}\nReply: {}",
                e.user.replace('\n', "\n  "),
                e.reply.replace('\n', "\n  ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Which side of an exchange a line continues.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    User,
    Reply,
}

/// `text` in the text form, read back into examples; an empty text is
/// none. The error names the line, or the exchange, that is wrong.
pub fn examples_from_text(text: &str) -> Result<Vec<Example>, String> {
    let mut out: Vec<Example> = Vec::new();
    // The exchange being read, and the side its last line belonged to.
    let mut cur: Option<(Example, Side)> = None;
    let mut blanks = 0usize;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        if line.trim().is_empty() && !line.starts_with("  ") {
            blanks += 1;
            continue;
        }
        if let Some(rest) = keyword(line, "user:") {
            if let Some((e, side)) = cur.take() {
                if side == Side::User {
                    return Err(format!(
                        "line {n}: a `User:` line before the previous exchange's `Reply:` line"
                    ));
                }
                out.push(e);
            }
            cur = Some((
                Example {
                    user: rest.to_string(),
                    reply: String::new(),
                },
                Side::User,
            ));
        } else if let Some(rest) = keyword(line, "reply:") {
            match cur.as_mut() {
                Some((e, side @ Side::User)) => {
                    e.reply = rest.to_string();
                    *side = Side::Reply;
                }
                _ => return Err(format!("line {n}: a `Reply:` line without a `User:` line")),
            }
        } else if line.starts_with([' ', '\t']) {
            let Some((e, side)) = cur.as_mut() else {
                return Err(format!(
                    "line {n}: an indented line before the first `User:` line"
                ));
            };
            let target = match side {
                Side::User => &mut e.user,
                Side::Reply => &mut e.reply,
            };
            target.push_str(&"\n".repeat(blanks + 1));
            target.push_str(unindent(line));
        } else {
            return Err(format!(
                "line {n}: expected `User:`, `Reply:` or a line indented by two spaces"
            ));
        }
        blanks = 0;
    }
    match cur {
        Some((_, Side::User)) => {
            return Err("the last exchange has no `Reply:` line".to_string());
        }
        Some((e, Side::Reply)) => out.push(e),
        None => {}
    }
    for (k, e) in out.iter_mut().enumerate() {
        e.user = e.user.trim().to_string();
        e.reply = e.reply.trim().to_string();
        let k = k + 1;
        if e.user.is_empty() {
            return Err(format!("exchange {k}: the `User:` side is empty"));
        }
        if e.reply.is_empty() {
            return Err(format!("exchange {k}: the `Reply:` side is empty"));
        }
    }
    Ok(out)
}

/// The rest of `line` after `kw` (lowercase, with its colon) at its start,
/// in any case.
fn keyword<'a>(line: &'a str, kw: &str) -> Option<&'a str> {
    let head = line.get(..kw.len())?;
    head.eq_ignore_ascii_case(kw).then(|| &line[kw.len()..])
}

/// A continuation line without its indent: two spaces, else one tab or one
/// space.
fn unindent(line: &str) -> &str {
    line.strip_prefix("  ")
        .or_else(|| line.strip_prefix('\t'))
        .or_else(|| line.strip_prefix(' '))
        .unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(user: &str, reply: &str) -> Example {
        Example {
            user: user.into(),
            reply: reply.into(),
        }
    }

    #[test]
    fn printing_then_reading_gives_the_examples_back() {
        let cases = vec![
            vec![],
            vec![ex("Hi", "Hello.")],
            vec![
                ex(
                    "Should I take an umbrella today?",
                    "I can't see the weather.",
                ),
                ex(
                    "Two lines?",
                    "First line.\nSecond line,\n  indented on its own.",
                ),
                ex("A gap", "Before.\n\nAfter the empty line."),
                ex("User: inside", "Reply: inside too\nUser: and here"),
            ],
        ];
        for examples in cases {
            let text = examples_to_text(&examples);
            assert_eq!(examples_from_text(&text).unwrap(), examples, "{text}");
        }
    }

    #[test]
    fn the_form_is_the_documented_one() {
        let text = examples_to_text(&[ex("A?", "B.\nC."), ex("D?", "E.")]);
        assert_eq!(text, "User: A?\nReply: B.\n  C.\n\nUser: D?\nReply: E.");
    }

    #[test]
    fn a_paste_that_lost_whitespace_still_reads() {
        // Keywords in any case, a tab indent, and the empty line of a text
        // whose two spaces an editor stripped.
        let text = "user: A?\nREPLY: B.\n\tC.\n\n  D.\n\n\nUser: E?\nReply: F.\n";
        assert_eq!(
            examples_from_text(text).unwrap(),
            vec![ex("A?", "B.\nC.\n\nD."), ex("E?", "F.")]
        );
    }

    #[test]
    fn what_is_wrong_is_named_by_line_or_exchange() {
        let e = examples_from_text("Reply: x").unwrap_err();
        assert!(e.contains("line 1"), "{e}");
        let e = examples_from_text("User: a\nUser: b\nReply: c").unwrap_err();
        assert!(e.contains("line 2"), "{e}");
        let e = examples_from_text("User: a").unwrap_err();
        assert!(e.contains("no `Reply:`"), "{e}");
        let e = examples_from_text("  stray").unwrap_err();
        assert!(e.contains("line 1"), "{e}");
        let e = examples_from_text("User: a\nReply: b\nstray").unwrap_err();
        assert!(e.contains("line 3"), "{e}");
        let e = examples_from_text("User: a\nReply: b\n\nUser:   \nReply: c").unwrap_err();
        assert!(e.contains("exchange 2"), "{e}");
    }
}
