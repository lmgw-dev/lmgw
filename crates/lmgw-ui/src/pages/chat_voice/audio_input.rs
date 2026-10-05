//! Voice turns the model hears, in the page (voice-audio-input design §2.3):
//! the thread's `voice_resolved.audio_input` — the setting, where it came
//! from, and the gateway's verdict — in the words the drawer, the chip and
//! the select show. The gateway's verdict is shown as it came, with one
//! exception the page sees before the gateway is asked again: the live GPU
//! block ([`AudioInputResolved::blocked`]), which keeps every chat model
//! lmgw runs from serving, as the STT and TTS chips read it.

use lmgw_api_types::chat_voice::{audio_input_label, AUDIO_INPUTS};
use serde::Deserialize;

use super::source_label;
use super::state::Block;

/// `voice_resolved.audio_input`: `value` (`off` | `local`) from `source`,
/// and the verdict — `path` (`audio` | `transcript`), the `model` the turn
/// goes to, and `why` the transcript.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AudioInputResolved {
    pub value: String,
    pub source: Option<String>,
    pub path: String,
    pub model: String,
    pub why: Option<String>,
    /// What blocks the model now, when the live block turned an audio
    /// verdict into the transcript ([`Self::blocked`]); never sent.
    #[serde(skip)]
    pub block: Option<&'static str>,
}

impl AudioInputResolved {
    /// The model hears the turn.
    pub fn hears(&self) -> bool {
        self.path == "audio"
    }

    /// The verdict as the live block leaves it (WP1 review M2): the GPU
    /// hold or a benchmark run keeps every chat model lmgw runs from serving
    /// — the gateway hands the turn to the model's fallback or refuses it —
    /// so an audio verdict goes as the transcript until the block ends.
    /// `None` when the block changes nothing.
    pub fn blocked(&self, b: Block) -> Option<Self> {
        let phrase = b.chat_phrase().filter(|_| self.hears())?;
        Some(Self {
            path: "transcript".into(),
            why: Some(format!(
                "{phrase} it goes to {}'s fallback, or is refused, as its transcript: your \
                 voice goes only to models lmgw runs",
                self.model
            )),
            block: Some(phrase),
            ..self.clone()
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

    /// Where the next voice turn goes: "your voice goes to … as audio", or
    /// "… gets the transcript: <why>" — under a block, the fallback does.
    pub fn verdict_line(&self) -> String {
        if self.hears() {
            return format!("your voice goes to {} as audio", self.model);
        }
        match (self.block, self.why.as_deref()) {
            (Some(_), Some(why)) => format!("the next voice turn: {why}"),
            (None, Some(why)) => format!("{} gets the transcript: {why}", self.model),
            (_, None) => format!("{} gets the transcript", self.model),
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
            format!(
                "Your voice goes to {} as audio (experimental). The speech-to-text model still \
                 transcribes each turn: the transcript is what is saved, and the reply waits \
                 for it.",
                self.model
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
    use serde_json::json;

    #[test]
    fn the_verdict_reads_the_servers_shape_and_says_where_the_voice_goes() {
        let hears: AudioInputResolved = serde_json::from_value(json!({
            "value": "local", "source": "chat", "path": "audio", "model": "gemma4-12b",
            "why": null
        }))
        .unwrap();
        assert!(hears.hears());
        assert_eq!(
            hears.verdict_line(),
            "your voice goes to gemma4-12b as audio"
        );
        assert_eq!(hears.chip_text(), "hears you");
        assert!(hears.effect_line().ends_with("· Settings → Chat"));

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

    /// The live block turns an audio verdict into the transcript before the
    /// gateway is asked again (WP1 review M2); not known is not blocked, and
    /// a transcript verdict keeps its own words.
    #[test]
    fn under_a_block_the_model_reads_the_transcript() {
        let hears: AudioInputResolved = serde_json::from_value(json!({
            "value": "local", "source": "chat", "path": "audio", "model": "gemma4-12b",
            "why": null
        }))
        .unwrap();
        let hold = Block {
            hold: true,
            ..Default::default()
        };
        let b = hears.blocked(hold).expect("blocked");
        assert_eq!(b.chip_text(), "reads the transcript");
        assert_eq!(b.block, Some("under the GPU hold"));
        assert_eq!(
            b.verdict_line(),
            "the next voice turn: under the GPU hold it goes to gemma4-12b's fallback, or is \
             refused, as its transcript: your voice goes only to models lmgw runs"
        );
        assert!(b
            .chip_title()
            .starts_with("Input: the next voice turn: under the GPU hold"));
        let bench = Block {
            benchmark: true,
            ..Default::default()
        };
        assert!(hears
            .blocked(bench)
            .unwrap()
            .verdict_line()
            .contains("while a benchmark run holds the GPU it goes to"));
        let unknown = Block {
            unknown: true,
            ..Default::default()
        };
        assert_eq!(hears.blocked(unknown), None, "not known is not amber");
        assert_eq!(hears.blocked(Block::default()), None);
        let off = AudioInputResolved {
            path: "transcript".into(),
            why: Some("audio input is off (this thread)".into()),
            ..hears
        };
        assert_eq!(off.blocked(hold), None);
    }
}
