//! The Prices page's list of hosted-tool fees (openai egress design §6.1):
//! the models whose native `/v1/responses` answers ran a hosted tool that
//! bills beyond its tokens, left unpriced for it.

use lmgw_api_types as dto;

use crate::pricing::HostedCalls;
use crate::state::SharedState;
use crate::store;

/// Every model whose answers were left unpriced by their hosted-tool calls,
/// one entry per alias and upstream model, with the requests and the calls
/// of each type since the first of them (read out of each row's note),
/// busiest first. Read from the raw log ([`store::hosted_fee_traffic`]), so
/// the log's retention bounds how far back it reaches. No price row fixes an
/// entry (lmgw has no price unit for those fees yet), so nothing drops off
/// before its traffic leaves the log.
pub async fn unpriced_hosted_tools(
    state: &SharedState,
) -> Result<Vec<dto::UnpricedHostedTool>, String> {
    let traffic = store::hosted_fee_traffic(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let mut out: Vec<(dto::UnpricedHostedTool, HostedCalls)> = Vec::new();
    for t in traffic {
        let scope_key = match (t.upstream_id, t.upstream_model.as_deref()) {
            (Some(up), Some(model)) => crate::config::upstream_scope_key(up, model),
            _ => String::new(),
        };
        let calls = HostedCalls::from_note(&t.price_note);
        let entry = match out
            .iter_mut()
            .find(|(e, _)| e.name == t.alias && e.scope_key == scope_key)
        {
            Some(entry) => entry,
            None => {
                out.push((
                    dto::UnpricedHostedTool {
                        name: t.alias.clone(),
                        scope_key,
                        since: t.ts.clone(),
                        ..Default::default()
                    },
                    HostedCalls::default(),
                ));
                out.last_mut().expect("just pushed")
            }
        };
        entry.0.requests += 1;
        if t.ts < entry.0.since {
            entry.0.since = t.ts;
        }
        for (ty, n) in calls.iter() {
            entry.1.add(ty, n);
        }
    }
    let mut out: Vec<dto::UnpricedHostedTool> = out
        .into_iter()
        .map(|(mut e, calls)| {
            e.calls = calls
                .iter()
                .map(|(ty, n)| dto::HostedCallCount {
                    item_type: ty.to_string(),
                    calls: n as i64,
                })
                .collect();
            e
        })
        .collect();
    out.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}
