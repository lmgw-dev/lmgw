//! Hosted-tool fees on the Prices page (openai egress design §6.1): the
//! pinned band of models whose native `/v1/responses` answers ran a hosted
//! tool that bills beyond its tokens (web search, file search, a container,
//! image generation).
//!
//! lmgw has no price unit for those fees yet, so the rows are unpriced and
//! count toward no budget. The band has no action: no price row fixes it.

use leptos::prelude::*;
use lmgw_api_types::UnpricedHostedTool;

use crate::fmt::grouped;

/// One model whose answers ran billable hosted tools, as its pinned row
/// prints it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct HostedFeeLine {
    pub name: String,
    pub scope_key: String,
    /// `2 web_search_call, 1 code_interpreter_call`.
    pub calls: String,
    pub requests: i64,
    /// The first unpriced request's time, as the log wrote it.
    pub since: String,
}

impl HostedFeeLine {
    pub(super) fn of(u: &UnpricedHostedTool) -> Self {
        Self {
            name: u.name.clone(),
            scope_key: u.scope_key.clone(),
            calls: u
                .calls
                .iter()
                .map(|c| format!("{} {}", grouped(c.calls.max(0) as u64), c.item_type))
                .collect::<Vec<_>>()
                .join(", "),
            requests: u.requests,
            since: u.since.clone(),
        }
    }
}

/// What the band's head says.
pub(super) fn band_meta(lines: &[HostedFeeLine]) -> String {
    let reqs: i64 = lines.iter().map(|l| l.requests.max(0)).sum();
    format!(
        "{} req ran hosted tools whose fees lmgw does not price yet · counted toward no budget",
        grouped(reqs as u64)
    )
}

/// A model whose answers made billable hosted calls: the calls, the
/// requests since the first, no action. The cells line up with the table's
/// columns (model, unit, tier, …).
#[component]
pub(super) fn HostedFeeRow(h: HostedFeeLine) -> impl IntoView {
    let since = h.since.get(..10).unwrap_or(&h.since).to_string();
    let tip = format!(
        "{} ({}) ran hosted tools that bill beyond their tokens: {} — lmgw has no price unit \
         for those fees yet, so these requests are unpriced since {}",
        h.name, h.scope_key, h.calls, h.since
    );
    view! {
        <tr class="unpriced-row">
            <td class="clip mono-sm" title=tip>{h.name.clone()}</td>
            <td class="dim clip" colspan="2" title="Hosted tool calls in those answers">
                {h.calls.clone()}
            </td>
            <td class="num" colspan="2">
                <span class="chip warn">
                    {format!("{} req unpriced", grouped(h.requests.max(0) as u64))}
                </span>
            </td>
            <td class="col-p2 dim" colspan="2">{format!("since {since}")}</td>
            <td class="col-p3"></td>
            <td class="actions"></td>
        </tr>
    }
}
