//! The account ops' passwords (headless server design §5.2, D26):
//! the transport rule, hashing under a permit, `account_password_set` and
//! account 1's first sign-in name.

use lmgw_api_types as dto;

use super::{account_of, caller_account, refused, reload};
use crate::accounts::{normalize_login, password, throttle, Account, ACCOUNT_ONE};
use crate::config::Snapshot;
use crate::principal::{Principal, Refusal};
use crate::state::SharedState;

/// How a request reached a password route (D26): its scheme, and whether
/// its peer is this machine with no proxy between.
#[derive(Debug, Clone, Copy)]
pub struct Transport {
    pub scheme: crate::net::Scheme,
    pub direct_loopback: bool,
}

/// D26: a password travels only over a secure transport. `403
/// login_insecure` on `http` from anything but a direct loopback peer
/// (a request through a trusted proxy is never loopback, D51).
pub fn require_secure(t: Transport) -> Result<(), Refusal> {
    if t.scheme == crate::net::Scheme::Https || t.direct_loopback {
        return Ok(());
    }
    Err(Refusal {
        status: axum::http::StatusCode::FORBIDDEN,
        code: "login_insecure",
        message: "a password is only taken over https or from this machine itself: open the \
                  dashboard on its TLS listener (Settings → Network & access → TLS) or behind \
                  a TLS proxy (proxy mode), or sign in with a one-time link (lmgw-server \
                  login-link on a server)"
            .into(),
    })
}

/// Hash `password` (checked against the rules first) on the blocking pool
/// under a verification permit (§5.10).
pub async fn hash_password(state: &SharedState, plain: &str) -> Result<String, Refusal> {
    let normalized = password::checked(plain)?;
    let verifier = &state.accounts.verifier;
    let permit = verifier
        .try_permit()
        .ok_or_else(|| password::busy(verifier.count()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        password::hash(&normalized)
    })
    .await
    .map_err(|e| Refusal::from(format!("hashing the password failed: {e}")))?
    .map_err(Refusal::from)
}

/// `account_password_set { id?, current?, password, login? }` (§5.2,
/// ruling 21).
///
/// For oneself: the current password when there is one and the caller is
/// a session, and the other sessions end. For another account: no current password, and all of
/// that account's sessions end. `login` changes account 1's sign-in name
/// while it has no password yet (§5.3, the only time it can).
pub async fn password_set(
    state: &SharedState,
    caller: &Principal,
    transport: Transport,
    args: PasswordSet,
) -> Result<dto::AccountDone, SetRefusal> {
    require_secure(transport)?;
    let me = caller_account(caller);
    let id = match args.id.or(me) {
        Some(id) => id,
        None => return Err(super::no_account().into()),
    };
    let account = account_of(&state.snapshot(), id)?;
    let login = first_login(&state.snapshot(), &account, args.login.as_deref())?;
    // The current password guards a session (a browser left open); a
    // bearer of the account's own owner key, the command line's door key
    // included, holds more than the password already.
    if let Some(session) = caller
        .session()
        .filter(|_| Some(id) == me && account.has_password)
    {
        check_current(state, &account, session, args.current.unwrap_or_default()).await?;
    }
    let hash = hash_password(state, &args.password).await?;
    crate::store::set_account_password(&state.db, id, &hash, login.as_deref())
        .await
        .map_err(|e| Refusal::from(e.to_string()))?;
    reload(state).await?;
    let except = (Some(id) == me).then(|| caller.session()).flatten();
    let ended = crate::accounts::sessions::end(
        state,
        crate::accounts::sessions::Which::Account {
            account_id: id,
            except,
        },
    )
    .await
    .map_err(Refusal::from)?;
    let who = login.as_deref().unwrap_or(&account.login).to_string();
    Ok(dto::AccountDone {
        message: if Some(id) == me {
            format!("your password is set; {ended} other session(s) of '{who}' ended")
        } else {
            format!("the password of '{who}' is set; its {ended} session(s) ended")
        },
        sessions_ended: ended as u64,
    })
}

/// A refusal of `account_password_set`, with the `Retry-After` its `429
/// login_throttled` (the current-password check's wait) and its `503
/// login_busy` carry, as the sign-in route's do (§5.2, §5.10).
#[derive(Debug)]
pub struct SetRefusal {
    pub refusal: Refusal,
    /// Whole seconds; `None` for a refusal that waits for nothing.
    pub retry_after: Option<u64>,
}

impl From<Refusal> for SetRefusal {
    fn from(refusal: Refusal) -> Self {
        let retry_after = (refusal.code == password::BUSY_CODE).then_some(1);
        Self {
            refusal,
            retry_after,
        }
    }
}

impl axum::response::IntoResponse for SetRefusal {
    fn into_response(self) -> axum::response::Response {
        let mut resp = self.refusal.into_response();
        if let Some(v) = self
            .retry_after
            .and_then(|s| axum::http::HeaderValue::from_str(&s.to_string()).ok())
        {
            resp.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, v);
        }
        resp
    }
}

/// `account_password_set`'s arguments.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PasswordSet {
    /// The account; left out = your own.
    pub id: Option<i64>,
    /// Your current password: needed to change your own when you have one.
    #[schemars(transform = lmgw_api_types::openapi_ext::secret)]
    pub current: Option<String>,
    /// The new password: at least 12 characters (Unicode code points after
    /// NFKC normalization), up to the 2 MiB body limit.
    #[schemars(transform = lmgw_api_types::openapi_ext::secret)]
    pub password: String,
    /// Account 1's sign-in name, changeable only with its first password.
    pub login: Option<String>,
}

/// The sign-in name a first password of account 1 may set (§5.3): `None`
/// to keep the login; refused for any other account and once account 1
/// has a password, unless it is the login it already has.
pub fn first_login(
    snap: &Snapshot,
    account: &Account,
    login: Option<&str>,
) -> Result<Option<String>, Refusal> {
    let Some(raw) = login.filter(|l| !l.trim().is_empty()) else {
        return Ok(None);
    };
    let login = normalize_login(raw)?;
    if login == account.login {
        return Ok(None);
    }
    if account.id != ACCOUNT_ONE || account.has_password {
        return Err(refused(
            "login_fixed",
            "a sign-in name is set when the account is made; account 1's can change only with \
             its first password",
        ));
    }
    if snap.account_by_login(&login).is_some() {
        return Err(refused(
            "login_taken",
            format!("there is an account '{login}' already"),
        ));
    }
    Ok(Some(login))
}

/// The current password of `account`, asked by its own session `session`:
/// throttled on the account's name (§5.4), so a session cannot try
/// passwords faster than a sign-in can, and verified under a permit.
async fn check_current(
    state: &SharedState,
    account: &Account,
    session: i64,
    current: String,
) -> Result<(), SetRefusal> {
    let rt = &state.accounts;
    let attempt = throttle::Attempt::current_password(&account.login, session);
    let reservation = rt
        .throttle
        .reserve(&attempt)
        .map_err(|(wait, held)| SetRefusal {
            refusal: throttle::throttled(wait, held),
            retry_after: Some(throttle::retry_after_secs(wait)),
        })?;
    let Some(permit) = rt.verifier.try_permit() else {
        rt.throttle.undo(reservation);
        return Err(password::busy(rt.verifier.count()).into());
    };
    let stored = match crate::store::account_password_hash(&state.db, account.id).await {
        Ok(h) => h,
        Err(e) => {
            rt.throttle.undo(reservation);
            return Err(Refusal::from(e.to_string()).into());
        }
    };
    let (v, _) = rt.verifier.verify(permit, current, stored).await;
    if v == password::Verified::Bad {
        // The reservation stays counted: a failure.
        return Err(Refusal {
            status: axum::http::StatusCode::UNAUTHORIZED,
            code: "password_wrong",
            message: "the current password is wrong".into(),
        }
        .into());
    }
    rt.throttle.succeeded(reservation);
    Ok(())
}
