//! Voice turns the model hears, in the page (voice-audio-input design §2.3):
//! the thread's `voice_resolved.audio_input` — the setting, where it came
//! from, and the gateway's verdict — in the words the drawer, the chip and
//! the select show. The gateway's verdict is shown as it came, with one
//! exception the page sees before the gateway is asked again: the live GPU
//! block ([`AudioInputResolved::blocked`]), which keeps every chat model
//! lmgw runs from serving, as the STT and TTS chips read it. Under it the
//! page shows the gateway's own forecast: the verdict of the fallback the
//! block hands the turn to, which hears it when it takes audio, wherever it
//! runs (changed 2026-10-06, decision D3).

use lmgw_api_types::chat_voice::{audio_input_label, AUDIO_INPUTS};
use serde::Deserialize;

use super::source_label;
use super::state::Block;

/// `voice_resolved.audio_input`: `value` (`off` | `on`) from `source`, and
/// the verdict — `path` (`audio` | `transcript`), the `model` the turn goes
/// to, `why` the transcript, the swap that handed it there (`lead`), and the
/// verdict a block would bring (`blocked`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AudioInputResolved {
    pub value: String,
    pub source: Option<String>,
    pub path: String,
    pub model: String,
    pub why: Option<String>,
    /// The gateway judged the verdict under a swap: the words for it
    /// ("under the GPU hold"). The verdict is the block's already.
    pub lead: Option<String>,
    /// The gateway's forecast for a GPU block (`blocked` on the wire): the
    /// fallback's own verdict, or the refusal when there is none — its
    /// `why` reads after the block's words ("this goes to …, which …").
    #[serde(rename = "blocked")]
    pub forecast: Option<Box<AudioInputResolved>>,
    /// What blocks the model now, when the live block put the forecast in
    /// the verdict's place ([`Self::blocked`]); never sent.
    #[serde(skip)]
    pub block: Option<&'static str>,
}

impl AudioInputResolved {
    /// The model hears the turn.
    pub fn hears(&self) -> bool {
        self.path == "audio"
    }

    /// The verdict as the live block leaves it (WP1 review M2, changed
    /// 2026-10-06): the GPU hold or a benchmark run keeps every chat model
    /// lmgw runs from serving, and the gateway hands the turn to the model's
    /// fallback or refuses it — so the page shows the gateway's forecast
    /// for that ([`Self::forecast`]), amber, until the gateway is asked
    /// again. `None` when the block changes nothing: none is live, the
    /// verdict was judged under it already, or no block touches the model.
    pub fn blocked(&self, b: Block) -> Option<Self> {
        let phrase = b.chat_phrase()?;
        if self.lead.is_some() {
            return None;
        }
        let f = self.forecast.as_deref()?;
        Some(Self {
            value: self.value.clone(),
            source: self.source.clone(),
            block: Some(phrase),
            ..f.clone()
        })
    }

    /// The setting in effect, as the drawer's other fields say theirs.
    pub fn effect_line(&self) -> String {
        format!(
            "in effect: {} · {}",
            audio_input_label(&self.value),
            source_label(self.source.as_deref())
        )
    }

    /// What handed the turn to `model`: the live block's words, else the
    /// gateway's for the swap it judged under.
    fn handed(&self) -> Option<&str> {
        match self.block {
            Some(phrase) => Some(phrase),
            None => self.lead.as_deref(),
        }
    }

    /// Where the next voice turn goes: "your voice goes to … as audio", or
    /// "… gets the transcript: <why>" — under a block, after its words, the
    /// fallback's verdict.
    pub fn verdict_line(&self) -> String {
        if self.hears() {
            return match self.handed() {
                Some(h) => format!("{h} your voice goes to {} as audio", self.model),
                None => format!("your voice goes to {} as audio", self.model),
            };
        }
        match (self.block, self.lead.is_some(), self.why.as_deref()) {
            (Some(phrase), _, Some(why)) => format!("the next voice turn: {phrase} {why}"),
            // The gateway's `why` starts with the swap's words.
            (None, true, Some(why)) => format!("the next voice turn: {why}"),
            (_, _, Some(why)) => format!("{} gets the transcript: {why}", self.model),
            (_, _, None) => format!("{} gets the transcript", self.model),
        }
    }

    /// The voice-mode chip's words.
    pub fn chip_text(&self) -> &'static str {
        if self.hears() {
            "hears you"
        } else {
            "reads the transcript"
        }
    }

    /// The chip's tooltip: the verdict, and what still holds on the audio
    /// path.
    pub fn chip_title(&self) -> String {
        if self.hears() {
            let line = self.verdict_line();
            let mut chars = line.chars();
            let line = match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect::<String>(),
                None => line,
            };
            format!(
                "{line} (experimental). The speech-to-text model still transcribes each turn: \
                 the transcript is what is saved, and the reply waits for it."
            )
        } else {
            let mut t = format!("Input: {}", self.verdict_line());
            if self.value == "off" {
                t.push_str(". Audio input is set in Settings → Chat → Voice or per thread.");
            }
            t
        }
    }
}

/// The thread's and the folder's select: inherit, then the two values.
pub(super) fn options() -> Vec<(String, String)> {
    std::iter::once((String::new(), "inherit".to_string()))
        .chain(
            AUDIO_INPUTS
                .iter()
                .map(|(n, l)| (n.to_string(), l.to_string())),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn the_verdict_reads_the_servers_shape_and_says_where_the_voice_goes() {
        let hears: AudioInputResolved = serde_json::from_value(json!({
            "value": "on", "source": "chat", "path": "audio", "model": "gemma4-12b",
            "why": null
        }))
        .unwrap();
        assert!(hears.hears());
        assert_eq!(
            hears.verdict_line(),
            "your voice goes to gemma4-12b as audio"
        );
        assert_eq!(hears.chip_text(), "hears you");
        assert!(hears
            .chip_title()
            .starts_with("Your voice goes to gemma4-12b as audio (experimental)."));
        assert_eq!(
            hears.effect_line(),
            "in effect: on: models that take audio (experimental) · Settings → Chat"
        );

        let off: AudioInputResolved = serde_json::from_value(json!({
            "value": "off", "source": "thread", "path": "transcript", "model": "gemma4-12b",
            "why": "audio input is off (this thread)"
        }))
        .unwrap();
        assert_eq!(
            off.verdict_line(),
            "gemma4-12b gets the transcript: audio input is off (this thread)"
        );
        assert_eq!(off.chip_text(), "reads the transcript");
        assert!(off.chip_title().contains("Settings → Chat → Voice"));
        assert_eq!(
            off.effect_line(),
            "in effect: off: the model reads the transcript · this thread"
        );
        assert_eq!(options()[0], (String::new(), "inherit".to_string()));
        assert_eq!(options().len(), 3);
    }

    /// A verdict the gateway judged under the hold names the swap, and a
    /// live block changes nothing more.
    #[test]
    fn a_verdict_judged_under_the_hold_names_the_fallback() {
        let hears: AudioInputResolved = serde_json::from_value(json!({
            "value": "on", "source": "chat", "path": "audio", "model": "openai/gpt",
            "why": null, "lead": "under the GPU hold"
        }))
        .unwrap();
        assert_eq!(
            hears.verdict_line(),
            "under the GPU hold your voice goes to openai/gpt as audio"
        );
        assert!(hears
            .chip_title()
            .starts_with("Under the GPU hold your voice goes to openai/gpt as audio"));
        let hold = Block {
            hold: true,
            ..Default::default()
        };
        assert_eq!(hears.blocked(hold), None, "the verdict is the block's");
        let texty = AudioInputResolved {
            path: "transcript".into(),
            why: Some(
                "under the GPU hold this goes to openai/gpt, which does not take audio input"
                    .into(),
            ),
            ..hears
        };
        assert_eq!(
            texty.verdict_line(),
            "the next voice turn: under the GPU hold this goes to openai/gpt, which does not \
             take audio input"
        );
    }

    /// Under a live block the page shows the gateway's forecast (decision
    /// D3): the fallback hears (amber "hears you"), reads the transcript, or
    /// the turn is refused; not known is not blocked, and a model no block
    /// touches keeps its verdict.
    #[test]
    fn under_a_block_the_page_shows_the_fallbacks_own_verdict() {
        let with = |forecast: Value| -> AudioInputResolved {
            serde_json::from_value(json!({
                "value": "on", "source": "chat", "path": "transcript", "model": "texty",
                "why": "texty does not take audio input", "blocked": forecast
            }))
            .unwrap()
        };
        let hold = Block {
            hold: true,
            ..Default::default()
        };
        let a = with(json!({ "path": "audio", "model": "openai/gpt", "why": null }));
        let b = a.blocked(hold).expect("blocked");
        assert_eq!(b.chip_text(), "hears you");
        assert_eq!(b.block, Some("under the GPU hold"));
        assert_eq!(
            b.verdict_line(),
            "under the GPU hold your voice goes to openai/gpt as audio"
        );
        assert_eq!(
            (b.value.as_str(), b.source.as_deref()),
            ("on", Some("chat"))
        );

        let a = with(json!({ "path": "transcript", "model": "openai/gpt",
                             "why": "this goes to openai/gpt, which does not take audio input" }));
        let bench = Block {
            benchmark: true,
            ..Default::default()
        };
        let b = a.blocked(bench).unwrap();
        assert_eq!(b.chip_text(), "reads the transcript");
        assert_eq!(
            b.verdict_line(),
            "the next voice turn: while a benchmark run holds the GPU this goes to openai/gpt, \
             which does not take audio input"
        );
        assert!(b
            .chip_title()
            .starts_with("Input: the next voice turn: while a benchmark run"));

        let a = with(json!({ "path": "transcript", "model": "texty",
                             "why": "texty has no fallback, so the turn is refused" }));
        assert_eq!(
            a.blocked(hold).unwrap().verdict_line(),
            "the next voice turn: under the GPU hold texty has no fallback, so the turn is \
             refused"
        );

        let unknown = Block {
            unknown: true,
            ..Default::default()
        };
        assert_eq!(a.blocked(unknown), None, "not known is not amber");
        assert_eq!(a.blocked(Block::default()), None);
        let cloud: AudioInputResolved = serde_json::from_value(json!({
            "value": "on", "source": "chat", "path": "audio", "model": "openai/gpt", "why": null
        }))
        .unwrap();
        assert_eq!(cloud.blocked(hold), None, "no block touches it");
    }
}
