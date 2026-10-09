//! The shared UI kit (personality-profiles design §5, D23).
//!
//! What lmgw's dashboard and its client apps have in common: the HTTP client,
//! the model catalog, formatting and persisted preferences, and the generic
//! widgets. It knows no router and no desktop shell, and it calls only the
//! gateway routes a picker needs (`/chat/api/profiles*`, `/v1/models`,
//! `/v1/audio/voices`), relative to the base [`http::configure`] sets;
//! `tests/network_contract.rs` enforces that on the sources.
//!
//! The design tokens and the widgets' rules are `assets/kit.css`; a host
//! links it before its own stylesheet.

pub mod catalog;
pub mod element_size;
pub mod fmt;
pub mod http;
pub mod prefs;
pub mod profiles;
pub mod scope;
pub mod widgets;
