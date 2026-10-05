//! What the core hands a heard response at its launch (voice-audio-input
//! design §3.1): its new turns as parts, and the journal's answer about its
//! user row — and those parts as the request carries them.

use base64::Engine as _;

use super::super::super::super::transcribe::Wav;
use crate::ir::ContentPart;
use crate::web::chat_voice::bound::RowWatch;

/// A heard response's audio input, as the core hands it over at launch
/// (§3.1): its new turns as parts — an audio turn as its WAV, the others as
/// their transcripts — and the journal's answer about its user row. Never
/// stored, and never logged.
pub(crate) struct Launch {
    pub parts: Vec<Spoken>,
    pub user_row: RowWatch,
}

/// One new turn of a heard response, as the core hands it over.
pub(crate) enum Spoken {
    /// An audio turn: its WAV, built once at the commit (maybe still being
    /// built).
    Wav(Wav),
    /// A turn transcribed before the launch: its words.
    Text(String),
    /// A part as it goes (the suite's seam).
    Ready(ContentPart),
}

impl Launch {
    /// The parts as the request carries them — an audio turn as its WAV in
    /// base64 — and why the turn cannot go as audio after all: its WAV
    /// could not be built (the ASR call then failed alike).
    pub(super) async fn prepare(self) -> (Heard, Option<String>) {
        let mut spoken = Vec::with_capacity(self.parts.len());
        let mut why = None;
        for p in self.parts {
            match p {
                Spoken::Wav(wav) => match wav.get().await {
                    Ok(bytes) => spoken.push(ContentPart::Audio {
                        mime: "audio/wav".into(),
                        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    }),
                    Err(e) => why = Some(format!("the turn's audio could not be prepared: {e}")),
                },
                Spoken::Text(text) if text.trim().is_empty() => {}
                Spoken::Text(text) => spoken.push(ContentPart::text(text)),
                Spoken::Ready(part) => spoken.push(part),
            }
        }
        let heard = Heard {
            spoken,
            user_row: self.user_row,
        };
        (heard, why)
    }
}

/// A heard response's parts as the request carries them, and its barrier.
pub(crate) struct Heard {
    pub spoken: Vec<ContentPart>,
    pub user_row: RowWatch,
}
