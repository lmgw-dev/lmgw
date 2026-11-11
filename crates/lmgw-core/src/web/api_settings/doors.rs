//! The key ops' account rules (headless server design §5.1): door keys
//! belong to their account, an owner name under `owner:account-` is the
//! server's to give, and every key a caller makes records who made it.

use crate::accounts::ops::caller_account;
use crate::principal::{Principal, Refusal};
use crate::state::SharedState;
use crate::store::KeyAccount;

fn conflict(code: &'static str, message: String) -> Refusal {
    Refusal {
        status: axum::http::StatusCode::CONFLICT,
        code,
        message,
    }
}

/// `key_reveal`, `key_rotate` and `key_set` on a door key that is not the
/// caller's account's, and `key_delete` on any door key (§5.1): refused,
/// naming the account. Any other owner key stays every account's (ruling
/// 6).
pub(super) fn refuse_door(
    st: &SharedState,
    caller: &Principal,
    op: &str,
    key_id: i64,
) -> Result<(), Refusal> {
    let snap = st.snapshot();
    let Some(account) = snap.accounts.iter().find(|a| a.door_key_id == key_id) else {
        return Ok(());
    };
    let name = crate::accounts::door_key_name(account.id);
    // `owner:dashboard`'s own refusal (principals §3.12) answers its delete.
    if op == "key_delete" && account.id == crate::accounts::ACCOUNT_ONE {
        return Ok(());
    }
    if op == "key_delete" {
        return Err(conflict(
            "door_key",
            format!(
                "'{name}' is the sign-in key of account '{}'; it goes with the account \
                 (account_delete)",
                account.login
            ),
        ));
    }
    if Some(account.id) != caller_account(caller) {
        return Err(conflict(
            "door_key",
            format!(
                "'{name}' is the sign-in key of account '{}'; change it through that account",
                account.login
            ),
        ));
    }
    Ok(())
}

/// `key_create kind=owner`'s name under the server's prefix: refused.
pub(super) fn refuse_reserved_name(name: &str) -> Result<(), Refusal> {
    if name.starts_with(crate::accounts::DOOR_KEY_PREFIX) {
        return Err(Refusal::from(format!(
            "owner key names starting with '{}' are the accounts' sign-in keys; pick another \
             name",
            crate::accounts::DOOR_KEY_PREFIX
        )));
    }
    Ok(())
}

/// The account a key the caller makes belongs to, with its login; `None`
/// for a caller that belongs to no account.
pub(super) fn maker(st: &SharedState, caller: &Principal) -> Option<(i64, Option<String>)> {
    let id = caller_account(caller)?;
    Some((id, st.snapshot().account(id).map(|a| a.login.clone())))
}

/// Record that key `id` was made by the caller (§5.1), and publish it. A
/// caller of no account records nothing: the key reads no "made by".
pub(super) async fn made(st: &SharedState, caller: &Principal, id: i64) -> Result<(), Refusal> {
    let Some((account, login)) = maker(st, caller) else {
        return Ok(());
    };
    crate::store::set_key_account(
        &st.db,
        id,
        KeyAccount {
            id: account,
            login: login.as_deref(),
        },
    )
    .await
    .map_err(|e| Refusal::from(e.to_string()))?;
    st.reload_snapshot()
        .await
        .map(|_| ())
        .map_err(|e| Refusal::from(e.to_string()))
}
