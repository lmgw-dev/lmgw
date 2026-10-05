//! Audio plumbing for the realtime route (realtime §4.1, §6.1, §8.2):
//! PCM16/WAV conversion ([`pcm`]), rate conversion ([`resample`]), the
//! longest pause in synthesized speech ([`pauses`]) and the Silero
//! voice-activity model ([`vad`]).

pub mod pauses;
pub mod pcm;
pub mod resample;
pub mod vad;
