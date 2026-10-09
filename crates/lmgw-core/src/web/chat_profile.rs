//! Personality profiles in a Chat turn (personality-profiles design §2):
//! which profile a thread's turn uses, and the system message and reasoning
//! assembled from it ([`assemble`]).
//!
//! A thread's profile is the snapshot's row its `profile_id` names; a
//! thread with none, or whose row is gone, has no profile, and its turns
//! are exactly what they were before profiles existed (D2): every part the
//! profile adds is absent, and the thread's own prompt is the base. The
//! snapshot holds every profile (D10), so a profile edit reaches the next
//! turn of every thread using it, a bound voice session's included (D21).
//!
//! What a profile changes is how the model talks, never what runs (D1):
//! - who the model is: a non-empty persona takes the thread prompt's place
//!   (D3) — the prompt stays stored and applies again without one;
//! - the length rule and the examples, after it, on every turn (D4, D5);
//! - a voice turn's block: the profile's own, or none, else the generic one
//!   (D6);
//! - the reasoning on/off, after the thread's own fields (D7).
//!
//! The voice fields (TTS alias, voice, speech style) are resolved with the
//! thread's voice settings, not here (`chat_voice::resolve`, D8).

/// The parts, their order and their precedence; a draft's [`assemble::Profile`]
/// assembles exactly as a stored one does (the editor's preview).
pub(crate) mod assemble;

pub(crate) use assemble::{system_message, Prompt};
