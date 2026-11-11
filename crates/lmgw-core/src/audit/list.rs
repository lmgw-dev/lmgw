//! `audit_list` (§5.8): the Accounts card's Activity list, a page of the
//! log at a time, everyone's or one account's, with the whole log's size
//! and its retention.

use serde::Deserialize;

use crate::principal::Refusal;
use crate::state::SharedState;
use crate::store::AuditWho;

/// Entries on a page when the caller names no count: the card's page.
pub const PAGE: u32 = 50;

/// `account`: an account's id, `"none"` for the entries of no account, or
/// `"everyone"` (the same as leaving it out).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AccountArg {
    Id(i64),
    Word(String),
}

/// `audit_list { account?, before?, count? }`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListArgs {
    pub account: Option<AccountArg>,
    pub before: Option<i64>,
    pub count: Option<u32>,
}

fn who(arg: Option<AccountArg>) -> Result<AuditWho, Refusal> {
    Ok(match arg {
        None => AuditWho::Everyone,
        Some(AccountArg::Id(id)) => AuditWho::Account(id),
        Some(AccountArg::Word(w)) => match w.as_str() {
            "everyone" => AuditWho::Everyone,
            "none" => AuditWho::NoAccount,
            other => {
                return Err(Refusal::from(format!(
                    "account must be an account's id, \"none\" or \"everyone\", not \"{other}\""
                )))
            }
        },
    })
}

/// One page, newest first.
pub async fn list(
    state: &SharedState,
    args: ListArgs,
) -> Result<lmgw_api_types::AuditPage, Refusal> {
    let count = args.count.unwrap_or(PAGE);
    if count == 0 {
        return Err(Refusal::from("count must be 1 or more".to_string()));
    }
    let who = who(args.account)?;
    let db = |e: crate::error::GatewayError| Refusal::from(e.to_string());
    let (mut entries, more) = crate::store::audit_page(&state.db, who, args.before, count)
        .await
        .map_err(db)?;
    for e in &mut entries {
        e.action_label = super::labels::action_label(&e.action);
    }
    let (total, bytes, oldest) = crate::store::audit_stats(&state.db).await.map_err(db)?;
    Ok(lmgw_api_types::AuditPage {
        entries,
        more,
        total,
        bytes,
        oldest,
        keep_days: state.snapshot().settings.accounts.audit_days,
        failed_writes: state
            .accounts
            .audit_failed
            .load(std::sync::atomic::Ordering::Relaxed),
    })
}
