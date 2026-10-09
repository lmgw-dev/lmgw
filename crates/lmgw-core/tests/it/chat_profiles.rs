//! Personality profiles (personality-profiles design §1, §3.1, §6).
//!
//! - `store`: the table and its writes — CRUD, the name rules, a built-in's
//!   absent fields following the built-in text, a delete clearing threads,
//!   folder defaults and the Chat's default in one transaction, re-creating
//!   a built-in, a tolerant read of the body, the thread and folder
//!   assignment, Keep, and the snapshot after a write (WP1);
//! - `routes`: `/chat/api/profiles*`'s CRUD rows over HTTP (WP4);
//! - `reset`: `POST /chat/api/profiles/{id}/reset`, a built-in back to its
//!   built-in texts;
//! - `folders`: a folder's null profile is Settings → Chat's `chat_profile`,
//!   and Admin Chat never takes that one;
//! - `speech_style`: "no speech style" through the self-admin tools;
//! - `stale`: a deleted profile's id never comes back (a folder write, a
//!   temporary thread, a settings save from a stale snapshot).

mod folders;
mod reset;
mod routes;
mod speech_style;
mod stale;
mod store;
