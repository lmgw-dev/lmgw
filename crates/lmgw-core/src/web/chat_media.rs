//! The files a Chat turn's image and audio tools make (chat-media-tools
//! design §6.3, §6.4).
//!
//! - [`ThreadFiles`] is the thread's side of the media tools' refs
//!   (`mcp::media::Refs`): `att:N` resolves to an attachment of this thread
//!   only, and a call's files are stored as the thread's attachments when it
//!   ends (`source = 'tool'`, its call id, no message yet), so the next call
//!   of the same turn can name them. The turn's persist binds them to its
//!   reply, or deletes them when the reply is not saved
//!   (`chat_turn::Turn::persist`).
//! - [`Hydrator`] turns the `lmgw:attachment/N` resources a turn record
//!   keeps back into pictures for the model that answers — the record never
//!   holds the bytes, which every open of the thread would carry.

//! - While a call runs ([`ThreadLive`]), its progress goes out as the
//!   turn's `progress` frames, and its own stop is registered for the
//!   per-call cancel route ([`cancel_route`]).

pub(crate) mod actions;
pub(in crate::web) mod cancel_route;
pub(crate) mod defaults_route;
mod progress;
mod rehydrate;
mod store;

pub(crate) use cancel_route::MediaCalls;
pub(crate) use progress::ThreadLive;
pub(crate) use rehydrate::Hydrator;
pub(crate) use store::ThreadFiles;
