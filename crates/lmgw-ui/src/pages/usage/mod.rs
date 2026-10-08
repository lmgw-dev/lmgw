//! Usage — what was spent, on what, how fast it answered, and how much of it
//! never left the house.
//!
//! The page is a *lens on the same rows* Traffic shows, not a parallel
//! universe with its own arithmetic: every chart reads `/api/usage/*`, which
//! reads the hourly rollups, so a chart and the log table can only disagree if
//! one of them is a bug. Visual contract: `docs/design/ui-rebuild/usage-mockup.html`;
//! rules: `docs/design/2026-09-18-usage-analytics-cost-policy-design.md`.
//!
//! Three routes: `/usage` is the charts under one filter row, `/usage/keys`
//! owns every key (create, revoke, policy, rotate) and `/usage/prices` the
//! price sheets — configuration that the filter row does not scope, so it is
//! not under it.
//!
//! Two rules from that spec drive most of the code here:
//!
//! * **Unknown is not zero** (§2.3). Every total that shows money shows its
//!   unpriced remainder beside it, and an unpriced share is drawn in the
//!   neutral hatch rather than as a zero-height segment.
//! * **Colour comes from the slot, never the rank** (§6.3). `SeriesMeta::slot`
//!   is assigned from the gateway's own sorted list of entities, so dropping a
//!   series with a filter cannot repaint the survivors.
//!
//! One page's worth of code, split by what it draws: [`shared`] holds the
//! fetch wrapper, time/money formatters and chart furniture every part below
//! reuses; [`tile_row`] and [`dashboard`] are the `/usage` overview; the
//! `cards_*` modules are its chart cards, grouped by what they chart; `keys`,
//! `key_line`, `key_editor`, `scope_editor` (the alias and tool pickers) and
//! `devices` (the paired client apps) are `/usage/keys`; `prices` is `/usage/prices`.

mod shared;
use shared::*;
pub(crate) use shared::{money, money_fine};

// The chart card's head and its Chart | Table twin, for the Benchmarks page's
// charts: one card furniture across the dashboard.
pub(crate) use shared::{chart_or_table, ChartHead};

// The billable quantities: how they read, what a remainder holds, and
// from when a window recorded them.
mod measured;
// Traffic names a row's quantities the way this page does.
pub(crate) use measured::quantity_texts;
use measured::*;

mod tile_row;
use tile_row::*;

mod dashboard;
pub use dashboard::Usage;
use dashboard::*;

mod cards_spend;
use cards_spend::*;

mod cards_traffic;
use cards_traffic::*;

mod cards_local;
use cards_local::*;

mod cards_errors;
use cards_errors::*;

mod keys;
pub use keys::UsageKeys;

mod key_line;
use key_line::*;

mod key_editor;
use key_editor::*;
mod scope_editor;

/// The Devices card on `/usage/keys` (client-apps design §1.4).
mod devices;
use devices::*;

mod price_units;
mod prices;
pub use prices::UsagePrices;
