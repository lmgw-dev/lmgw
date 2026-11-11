//! `lmgw-server`: lmgw's production start without the tray app, for the
//! server image (headless server design §1, D1). No Tauri and no WebKit; the
//! dashboard comes embedded through `lmgw-core` as it does for the tray app.
//!
//! It starts where `LMGW_DEPLOYMENT=container` (the image sets it) or
//! `LMGW_DEV` is set, and refuses anywhere else. A library beside the binary
//! so its integration tests drive the same steps the binary takes.

// The headless server image is Linux: on any other system the library is empty and the binary
// says it runs on Linux only (not on Windows yet).
#![cfg(target_os = "linux")]

pub mod account;
pub mod cli;
pub mod env;
pub mod start;
