//! Hosted-tool fees (openai egress design §6.1, WP5b).
//!
//! A tool the upstream runs itself can bill per call or per session on top
//! of the tokens, and that fee is in no `usage` object: OpenAI's pricing page
//! (checked 2026-10-24) charges web search per 1k calls, file search per 1k
//! calls, Code Interpreter and Hosted Shell per container session, and image
//! generation at the image model's own token rates. lmgw has no price unit
//! for any of them yet, so a row whose answer made such a call is
//! **unpriced**: cost NULL, never the token part alone, which would read as
//! the whole bill and undercount it (billable-units design: one unknown part
//! makes the whole unknown). Its `price_note` names the calls and what the
//! tokens alone would have cost, and the Prices page lists such rows
//! (`ops::unpriced_hosted_tools`).
//!
//! Budgets count that token part as a lower bound
//! (`pricing::Cost::budget_floor_micro`): otherwise a budgeted client key
//! could name a hosted tool on every request, leave every row unpriced and
//! spend tokens without limit. The row, its totals and its rollups' cost
//! stay unpriced; only the budget spend reads the bound, and the note says
//! so.

use crate::pricing::micro_to_units;

/// How a note about hosted-tool fees begins; the Prices page's list finds
/// such rows by it (`store::hosted_fee_traffic`).
pub const HOSTED_FEES_NOTE: &str = "hosted tool fees not priced yet";

/// The billable hosted-tool calls of one answer: each output item type with
/// its count, in the order first seen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostedCalls(Vec<(String, u64)>);

impl HostedCalls {
    /// Count one call of item type `ty`.
    pub fn add(&mut self, ty: &str, n: u64) {
        match self.0.iter_mut().find(|(t, _)| t == ty) {
            Some((_, c)) => *c += n,
            None => self.0.push((ty.to_string(), n)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Each item type with its count.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.0.iter().map(|(t, n)| (t.as_str(), *n))
    }

    /// The row's note: "hosted tool fees not priced yet: 2 web_search_call,
    /// 1 code_interpreter_call; tokens alone would be 0.002750 USD, which
    /// budgets count". `tokens_micro` is what the row's tokens cost by
    /// themselves, which budgets count as a lower bound; `None` when they
    /// are unpriced too.
    pub fn note(&self, tokens_micro: Option<i64>, currency: &str) -> String {
        let calls = self
            .iter()
            .map(|(t, n)| format!("{n} {t}"))
            .collect::<Vec<_>>()
            .join(", ");
        let tokens = match tokens_micro {
            Some(m) => format!(
                "tokens alone would be {:.6} {currency}, which budgets count",
                micro_to_units(m)
            ),
            None => "the tokens are unpriced as well".to_string(),
        };
        format!("{HOSTED_FEES_NOTE}: {calls}; {tokens}")
    }

    /// The calls a row's `price_note` names, read back from [`Self::note`]'s
    /// words wherever in the note they stand. Empty for a note without them.
    pub fn from_note(note: &str) -> Self {
        let mut calls = Self::default();
        let Some(at) = note.find(HOSTED_FEES_NOTE) else {
            return calls;
        };
        let rest = &note[at + HOSTED_FEES_NOTE.len()..];
        let Some(list) = rest.strip_prefix(": ") else {
            return calls;
        };
        let list = list.split(';').next().unwrap_or_default();
        for entry in list.split(", ") {
            if let Some((n, ty)) = entry.trim().split_once(' ') {
                if let Ok(n) = n.parse::<u64>() {
                    calls.add(ty, n);
                }
            }
        }
        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_note_names_the_calls_and_the_token_part_and_reads_back() {
        let mut c = HostedCalls::default();
        c.add("web_search_call", 1);
        c.add("code_interpreter_call", 1);
        c.add("web_search_call", 1);
        let note = c.note(Some(2_750), "USD");
        assert_eq!(
            note,
            "hosted tool fees not priced yet: 2 web_search_call, 1 code_interpreter_call; \
             tokens alone would be 0.002750 USD, which budgets count"
        );
        assert_eq!(HostedCalls::from_note(&note), c);
        // Behind another note, as a tier gap's.
        let both = format!("served at service tier flex; no flex price on file; {note}");
        assert_eq!(HostedCalls::from_note(&both), c);
        assert!(c
            .note(None, "EUR")
            .ends_with("the tokens are unpriced as well"));
        assert!(
            HostedCalls::from_note("stream ended before the upstream reported usage").is_empty()
        );
    }
}
