//! The Voices lab: fixed voices for the cloning text-to-speech models.
//!
//! Pick any text-to-speech model, describe or pick a voice, and Generate
//! renders a round of candidates of the language's sentence, each with its
//! own seed where the engine takes one. Listen, save the one you like to the
//! voice library under a name — its leading and trailing silence cut, the
//! text it says recorded as its transcript — and have a cloning model speak
//! a test line with it. The library (the reference panel) shows how each
//! sample was made, and Regenerate puts that back into the form.
//!
//! The page posts to `/voice-lab/api/*` (`lmgw_core::voice_lab`): a round
//! goes in process through the speech path a realtime answer uses, so the
//! hold, its fallback, shaping and the log row are real; the rounds are kept
//! on the server until discarded.

/// The cloning check card.
mod clone_check;
/// The form column: voice, text, candidates.
mod form;
/// The head line: the model and what it takes.
mod head;
/// The reference panel: the voice library with each sample's provenance.
mod library;
/// The page's state, its calls and its wiring onto the labs' layout.
mod page;
/// The results column: the rounds and their candidates, Save.
mod rounds;

pub use page::VoiceLab;
