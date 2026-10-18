//! The two ops that act on the stored `/v1/responses` conversations, as the
//! dashboard's op dispatcher calls them. Both say what they did in a
//! [`dto::MessageAck`].

use lmgw_api_types as dto;

use super::Args;
use crate::state::SharedState;

/// `response_chain_delete { chain_id }`: every stored response of one chain.
pub(super) async fn response_chain_delete(
    st: &SharedState,
    args: &Args,
) -> Result<dto::MessageAck, String> {
    let chain_id = super::arg_str(args, "chain_id").ok_or("pass chain_id")?;
    let n = crate::store::delete_response_chain(&st.db, chain_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(dto::MessageAck::new(format!("deleted {n} response(s)")))
}

/// `responses_gc { scope }`: `all` clears the store; anything else applies the
/// retention settings.
pub(super) async fn responses_gc(st: &SharedState, args: &Args) -> Result<dto::MessageAck, String> {
    let scope = super::arg_str(args, "scope").unwrap_or("rules");
    if scope == "all" {
        let chains = crate::store::list_response_chains(&st.db, i64::MAX)
            .await
            .unwrap_or_default();
        let mut n = 0;
        for c in &chains {
            n += crate::store::delete_response_chain(&st.db, &c.chain_id)
                .await
                .unwrap_or(0);
        }
        return Ok(dto::MessageAck::new(format!(
            "cleared {n} stored response(s)"
        )));
    }
    let s = st.snapshot().settings.clone();
    match crate::store::gc_responses(&st.db, s.responses_retention_hours, s.responses_max_chains)
        .await
        .map_err(|e| e.to_string())?
    {
        0 => Ok(dto::MessageAck::new("nothing to evict")),
        n => Ok(dto::MessageAck::new(format!("evicted {n} response(s)"))),
    }
}
