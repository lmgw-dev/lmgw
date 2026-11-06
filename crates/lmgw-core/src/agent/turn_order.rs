//! The order of a turn's parts, for a turn that carries reasoning seals
//! (openai egress design §7.3.1), Anthropic thinking signatures or verbatim
//! Anthropic blocks: one reasoning item is one [`ContentPart::Reasoning`]
//! part, where it stood among the turn's messages and calls. OpenAI refuses
//! a replayed reasoning item without the item that followed it ("Item
//! 'rs_…' of type 'reasoning' was provided without its required following
//! item."), so a sealed turn is replayed in the order it was answered; a
//! signed thinking block, and a block the IR does not model
//! ([`AnthropicBlock`]), goes back to Anthropic where it stood, and so does a
//! cited text block, its text with its citations. A turn with
//! none of them keeps the shape it always had
//! ([`super::TurnAccumulator::finish`]): its text, its refusal, its calls,
//! and its reasoning apart.

use crate::ir::{AnthropicBlock, BlockEvent, ContentPart, Seal, Signer};

/// What closed a reasoning item: its OpenAI seal or Anthropic's signature,
/// with the scope that signed it.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Close {
    Seal(Seal),
    Signature(String, Signer),
}

/// One stretch of a turn, in the order the stream gave it.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Piece {
    /// A text block's text so far, and its citations with the scope that
    /// answered once the block ended (`None` while it is open): a text after
    /// its end is another block's.
    Text(String, Option<(Vec<serde_json::Value>, Signer)>),
    Refusal(String),
    /// A reasoning item's text so far, and what closed it once it came.
    Reasoning(String, Option<Close>),
    /// The call in this slot of the accumulator's calls.
    Call(usize),
    /// A verbatim Anthropic block, its deltas folded in as they came.
    Block(AnthropicBlock),
}

/// The turn's pieces, and whether any reasoning item was sealed or signed
/// or any verbatim block came.
#[derive(Debug, Default)]
pub(super) struct TurnOrder {
    pieces: Vec<Piece>,
    in_place: bool,
}

impl TurnOrder {
    pub(super) fn text(&mut self, t: &str) {
        match self.pieces.last_mut() {
            Some(Piece::Text(s, None)) => s.push_str(t),
            _ => self.pieces.push(Piece::Text(t.to_string(), None)),
        }
    }

    /// A text block's end, with its citations
    /// ([`crate::ir::StreamDelta::TextBlockEnd`]), from the row whose scope
    /// `scope` names: the open text is that block's, and a text after it
    /// another's. Citations make the turn keep its parts in place; with no
    /// text open they are a text part of their own, which the Anthropic
    /// egress does not send (an empty text block is refused) and names
    /// ([`crate::ir::AnthropicSent`]).
    pub(super) fn text_end(&mut self, c: &[serde_json::Value], scope: &Signer) {
        self.in_place |= !c.is_empty();
        let end = Some((c.to_vec(), scope.clone()));
        match self.pieces.last_mut() {
            Some(Piece::Text(_, open @ None)) => *open = end,
            _ if !c.is_empty() => self.pieces.push(Piece::Text(String::new(), end)),
            _ => {}
        }
    }

    pub(super) fn refusal(&mut self, t: &str) {
        match self.pieces.last_mut() {
            Some(Piece::Refusal(s)) => s.push_str(t),
            _ => self.pieces.push(Piece::Refusal(t.to_string())),
        }
    }

    /// A reasoning delta: more of the open item, or the first of a new one.
    /// A new item that follows an unsigned or sealed one starts without the
    /// blank line the stream put between them: that line joins the turn's
    /// whole trace (the platform decoder's). Anthropic's thinking is never
    /// joined so, and its text, which its signature covers, is kept as it
    /// came: after a signed block, and as the turn's first item.
    pub(super) fn reasoning(&mut self, r: &str) {
        if let Some(Piece::Reasoning(s, None)) = self.pieces.last_mut() {
            s.push_str(r);
            return;
        }
        let joined = self.pieces.iter().rev().find_map(|p| match p {
            Piece::Reasoning(_, close) => Some(!matches!(close, Some(Close::Signature(..)))),
            _ => None,
        });
        let r = match joined {
            Some(true) => r.trim_start_matches('\n'),
            _ => r,
        };
        self.pieces.push(Piece::Reasoning(r.to_string(), None));
    }

    /// The open reasoning item's seal, which closes it; an item that came
    /// with no summary text is a part of its own.
    pub(super) fn seal(&mut self, s: &Seal) {
        self.close(Close::Seal(s.clone()));
    }

    /// The open thinking block's signature from the scope `scope` names,
    /// which closes it; a block that streamed no thinking text (an omitted
    /// display) is a part of its own.
    pub(super) fn signature(&mut self, s: &str, scope: &Signer) {
        self.close(Close::Signature(s.to_string(), scope.clone()));
    }

    fn close(&mut self, c: Close) {
        self.in_place = true;
        match self.pieces.last_mut() {
            Some(Piece::Reasoning(_, open @ None)) => *open = Some(c),
            _ => self.pieces.push(Piece::Reasoning(String::new(), Some(c))),
        }
    }

    pub(super) fn call(&mut self, slot: usize) {
        self.pieces.push(Piece::Call(slot));
    }

    /// A verbatim block's event: its start is a piece of its own, a delta
    /// is folded into the block it streams ([`AnthropicBlock::apply_delta`]);
    /// a delta with no block begun has none to go to.
    pub(super) fn block(&mut self, ev: &BlockEvent) {
        match ev {
            BlockEvent::Start(b) => {
                self.in_place = true;
                self.pieces.push(Piece::Block(b.clone()));
            }
            BlockEvent::Delta(d) => {
                if let Some(Piece::Block(b)) = self.pieces.last_mut() {
                    b.apply_delta(d);
                }
            }
            // Whole already: every delta was folded in as it came.
            BlockEvent::Stop => {}
        }
    }

    /// The pieces, when a seal, a signature or a verbatim block came;
    /// `None` for a turn that keeps the shape it always had (module doc).
    pub(super) fn sealed_pieces(self) -> Option<Vec<Piece>> {
        self.in_place.then_some(self.pieces)
    }
}

/// A sealed or signed turn's parts in order: `call` turns a call's slot
/// into its part. Empty texts and empty open reasoning are left out.
pub(super) fn parts(
    pieces: Vec<Piece>,
    mut call: impl FnMut(usize) -> Option<ContentPart>,
) -> Vec<ContentPart> {
    pieces
        .into_iter()
        .filter_map(|p| match p {
            Piece::Text(text, end) => {
                let (citations, scope) = end.map_or((Vec::new(), None), |(c, s)| (c, Some(s)));
                (!text.is_empty() || !citations.is_empty())
                    .then(|| ContentPart::cited(text, citations, scope))
            }
            Piece::Refusal(t) if !t.is_empty() => Some(ContentPart::Refusal { text: t }),
            Piece::Reasoning(t, Some(Close::Seal(s))) => Some(ContentPart::sealed_reasoning(t, s)),
            Piece::Reasoning(t, Some(Close::Signature(s, scope))) => {
                Some(ContentPart::signed_reasoning(t, s, Some(scope)))
            }
            Piece::Reasoning(t, None) if !t.is_empty() => Some(ContentPart::reasoning(t)),
            Piece::Call(slot) => call(slot),
            Piece::Block(b) => Some(ContentPart::AnthropicBlock(b)),
            _ => None,
        })
        .collect()
}
