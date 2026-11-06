//! A tool turn refused before its loop's first model call — at admission,
//! or on the route admission settled on (`fit_route`: a continue with no
//! prefill, a heard turn's audio on a route whose model cannot take it) — writes its
//! request row as the plain path does (`chat::relay`, voice-audio-input
//! WP2 review #8): the refusal is model traffic that did not happen, and
//! the Requests page says why. The profile editor's Test
//! (`chat_profiles::try`) records its refusal at admission the same way.

pub(in crate::web) use crate::agent::claim::refused::record;
