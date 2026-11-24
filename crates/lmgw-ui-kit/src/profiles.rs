//! The personality-profile editor (personality-profiles design §4.2, decision 7).
//!
//! One component for lmgw's dashboard and the desktop client's settings
//! window alike. It reaches the gateway only through `/chat/api/profiles*`
//! (plus the pickers' `/v1/models` and `/v1/audio/voices`), relative to the
//! base [`crate::http::configure`] sets.
//!
//! - [`ProfileEditor`]: one profile (or a new one) as preference groups
//!   with the save toast: fields, the static part with Count, Test, Speak a
//!   sample, an optional hero with the built-in, usage and default chips,
//!   Reset, and a delete that asks in a dialog what it clears.
//! - [`ProfilesPanel`]: the list with the editor beside it, for a settings
//!   tab that has nothing else to show (the dashboard has its own list and
//!   detail on the shared split view).
//! - [`upsert`]: a stored row into a list, in the gateway's order.
//!
//! The state and the wire shaping are in [`model`] and [`api`] and tested
//! without a browser. A host links `assets/profiles.css` after `kit.css`.

pub mod api;
mod editor;
mod fields;
pub mod model;
mod panel;
mod try_panel;

pub use editor::ProfileEditor;
pub use panel::{upsert, ProfilesPanel};
