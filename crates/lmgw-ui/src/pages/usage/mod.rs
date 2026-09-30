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
//! `key_line`, `key_editor` and `scope_editor` (the alias and tool pickers) are `/usage/keys`; `prices` is `/usage/prices`.

mod shared;
pub(crate) use shared::money;
use shared::*;

// The chart card's head and its Chart | Table twin, for the Benchmarks page's
// charts: one card furniture across the dashboard.
pub(crate) use shared::{chart_or_table, ChartHead};

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

mod prices;
pub use prices::UsagePrices;
