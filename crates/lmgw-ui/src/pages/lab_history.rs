//! The lab store's client (labs-store design §1.5, §2.9): one history per
//! lab, read in pages from `/labs/api/{lab}/groups`, its usage, deletes,
//! Clear and the export hrefs. The Decision lab is its first user; the Audio
//! and Image labs take it in slice 4, each rendering a group through its own
//! card.

/// The history's state and every call to the shared `/labs/api/{lab}/*`
/// routes.
mod client;
/// The results head's Export menu.
mod export_menu;
/// The history list: the lab's cards, newest first, and the older pages.
mod list;
/// What a kept lab's results head says about its size.
mod usage_line;

pub(super) use client::{export_href, runless, ExportScope, LabHistory, Runless};
pub(super) use export_menu::ExportMenu;
pub(super) use list::HistoryList;
pub(super) use usage_line::{clear_confirm, kept_note, kept_title};
