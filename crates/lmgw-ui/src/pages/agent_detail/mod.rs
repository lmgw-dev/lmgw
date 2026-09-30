//! One agent (agent-catalog design §6.2): Run, Runs, App, Definition — each a
//! path of its own (`/agents/<id>`, `/agents/<id>/runs`, …), so a tab is
//! bookmarkable and Back goes to the tab before.
//!
//! **Run** is the config form (§2.6) beside the run column: whatever the run
//! kind offers, in view while the form scrolls. For `chat` that is "Open in
//! Chat", which is one op: the thread is seeded server-side from the manifest
//! — model, system prompt, tools, temperature — and the page navigates to it.
//! For `batch` it is the two Start buttons. Once a run is on screen the tab is
//! its **review**: the progress line, the table with attention first, and
//! Apply behind a confirmation (§6.2), with the config folded to a panel.
//!
//! **Runs** is what this agent has done: its threads for a `chat` agent, its
//! `agent_run` jobs for a `batch` one, any of which reopens its review table.
//!
//! **Every piece of run state lives on the page, not in a tab.** A tab body is
//! rebuilt on every tab click, so a signal declared inside `RunTab` is wiped by
//! a glance at Runs — including the rows of a run still in flight and the
//! checkboxes someone has been ticking for a minute. [`RunState`] is the whole
//! surface as one `Copy` bundle, owned by [`AgentDetailPage`] and handed down.
//!
//! **Definition** is the manifest in a textarea with Validate, Save, Export,
//! Duplicate, Reset and Delete. The manifest travels as **text**, never as a
//! parsed object: `serde_json::Map` is a `BTreeMap`, so a round trip through a
//! JSON value alphabetizes the config schema's properties and silently re-sorts
//! the form the author laid out (§2.6). `agent_set` accepts an object and says
//! it re-sorted; this page never makes it say that.
//!
//! One page's worth of code, split by what it draws: [`shared`] holds the
//! pure formatting helpers every tab reuses; [`run_state`] is the run
//! surface's state (§6.2), owned by the page and handed down; [`page`] is
//! [`AgentDetailPage`] itself; [`run_tab`] is the Run tab's config form and
//! run column; [`runtime`] is the Runtime block; [`review_pane`] and
//! [`review_row`] are the batch run surface (§6.2) a run opens into;
//! [`app_tab`] is App; [`runs_tab`] is Runs/Threads; [`definition_tab`] is
//! Definition.

mod shared;
pub(crate) use shared::unbound_line;
use shared::*;

mod run_state;
use run_state::*;

mod page;
pub use page::AgentDetailPage;
use page::*;

mod run_tab;
use run_tab::*;

mod runtime;
use runtime::*;

mod review_pane;
use review_pane::*;

mod review_row;
use review_row::*;

mod app_tab;
use app_tab::*;

mod runs_tab;
use runs_tab::*;

mod definition_tab;
use definition_tab::*;

#[cfg(test)]
mod tests;
