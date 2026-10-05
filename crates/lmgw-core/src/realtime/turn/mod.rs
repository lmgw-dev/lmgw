//! Turn taking (realtime §6): the `server_vad` end-of-turn detector
//! ([`server_vad`]), the barge-in evidence gate that decides whether
//! speech during playback interrupts the response ([`barge_in`]), the
//! arbiter that hands each frame to one of the two ([`arbiter`]), and what
//! the barge-in word check counts as a backchannel ([`backchannel`]) and as
//! words at all ([`scripts`]). `semantic_vad` (§6.3) is the detector with
//! Smart Turn ([`smart_turn`], on Whisper features, [`mel`]) deciding when
//! a pause ends the turn ([`semantic`]).
//!
//! The detector and the gate are driven by per-frame Silero probabilities
//! on the session's 24 kHz input timeline; neither touches a model or the
//! clock itself. The rule asks for a Smart Turn score and takes the answer
//! back, so the model runs wherever the session puts it (`spawn_blocking`).

pub mod arbiter;
pub mod backchannel;
pub mod barge_in;
pub mod mel;
pub mod scripts;
pub mod semantic;
pub mod server_vad;
pub mod smart_turn;
