//! The characters a response's clauses had replaced or dropped because the
//! row's engine cannot say them (`crate::audio::charset`): gathered clause
//! by clause and said in one log line when the response ends — a realtime
//! answer's and a Chat read-aloud's alike. A line per clause would repeat
//! the same quote mark for every sentence of a German reply.

use crate::audio::charset::named;
use crate::audio::shape::{ShapeChange, ShapeReport};

/// Each codepoint once, in the order the response first had it.
#[derive(Debug, Default)]
pub(crate) struct CharsSeen {
    replaced: Vec<u32>,
    dropped: Vec<u32>,
}

impl CharsSeen {
    /// What shaping changed in one clause.
    pub fn note(&mut self, shaped: &ShapeReport) {
        for change in &shaped.changes {
            if let ShapeChange::Chars { replaced, dropped } = change {
                add(&mut self.replaced, replaced);
                add(&mut self.dropped, dropped);
            }
        }
    }

    /// Logs every character `report` says shaping fitted out, in full, as
    /// one line `who` opens (`speech: TTS 'x'`, `task: 'x'`); nothing when
    /// every character went as it came. One line for a request that is
    /// refused for its characters or sent; none when admission fails first.
    pub fn log_once(report: &ShapeReport, who: &str) {
        let mut seen = Self::default();
        seen.note(report);
        if let Some(line) = seen.line() {
            tracing::info!("{who}: characters its engine cannot say: {line}");
        }
    }

    /// The log line's text (`replaced U+201E „; dropped U+1F60A 😊`);
    /// `None` when every character went as it came.
    pub fn line(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.replaced.is_empty() {
            parts.push(format!("replaced {}", named(&self.replaced)));
        }
        if !self.dropped.is_empty() {
            parts.push(format!("dropped {}", named(&self.dropped)));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

fn add(into: &mut Vec<u32>, from: &[u32]) {
    for cp in from {
        if !into.contains(cp) {
            into.push(*cp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(replaced: &[u32], dropped: &[u32]) -> ShapeReport {
        ShapeReport {
            changes: vec![
                ShapeChange::Tags {
                    mapped: 0,
                    stripped: 1,
                },
                ShapeChange::Chars {
                    replaced: replaced.to_vec(),
                    dropped: dropped.to_vec(),
                },
            ],
        }
    }

    #[test]
    fn a_response_s_changes_are_one_line_each_codepoint_once() {
        let mut seen = CharsSeen::default();
        assert_eq!(seen.line(), None);
        seen.note(&ShapeReport::default());
        assert_eq!(seen.line(), None, "a clause said as it came");
        seen.note(&chars(&[0x201E], &[]));
        seen.note(&chars(&[0x201E, 0x2028], &[0x1F60A]));
        assert_eq!(
            seen.line().unwrap(),
            "replaced U+201E \u{201E}, U+2028; dropped U+1F60A \u{1F60A}"
        );
        let mut seen = CharsSeen::default();
        seen.note(&chars(&[], &[0x0301]));
        assert_eq!(seen.line().unwrap(), "dropped U+0301");
    }
}
