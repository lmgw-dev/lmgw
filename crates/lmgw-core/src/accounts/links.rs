//! Set-password links (headless server design §5.5, D15): no e-mail; a new
//! account and a password reset each get a one-time link the admin hands
//! over. The token sits in the fragment (`#token=`), so it reaches neither
//! a server log nor a `Referer`; the link's validity is printed with it.
//! One link per account: a second replaces the first.

use std::time::Duration;

use crate::principal::Refusal;
use crate::state::SharedState;

/// How long a set-password link works (§5.5).
pub const PASSWORD_LINK_TTL: Duration = Duration::from_secs(24 * 3600);

/// The page a link opens.
pub const SET_PASSWORD_PATH: &str = "/set-password";

/// A minted link.
#[derive(Debug, Clone)]
pub struct Link {
    pub url: String,
    /// RFC 3339 UTC.
    pub expires_at: String,
    /// "valid until 2026-11-09 14:00 UTC".
    pub valid_until: String,
}

/// "valid until <date> UTC" for an RFC 3339 instant.
pub fn valid_until(at: chrono::DateTime<chrono::Utc>) -> String {
    format!("valid until {}", at.format("%Y-%m-%d %H:%M UTC"))
}

/// The public origin links are built from (§4.3): `public_url`, else the
/// TLS listener that serves, else the plain listener.
pub fn public_base(state: &SharedState) -> String {
    let snap = state.snapshot();
    crate::net::public_base_url(&snap.settings, snap.serving_tls.as_ref())
}

/// The expiry `ttl` from now: RFC 3339 UTC, and "valid until …".
pub fn expiry_after(ttl: Duration) -> (String, String) {
    let at = chrono::Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default();
    (
        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        valid_until(at),
    )
}

/// Mint a set-password link for `account_id`, replacing any earlier one.
pub async fn mint(state: &SharedState, account_id: i64) -> Result<Link, String> {
    mint_on(state, account_id, &public_base(state)).await
}

/// [`mint`] on the origin `base`: the start's line builds it on the
/// address this start bound.
pub async fn mint_on(state: &SharedState, account_id: i64, base: &str) -> Result<Link, String> {
    let (token, hash) = crate::accounts::mint_secret();
    let (expires_at, valid_until) = expiry_after(PASSWORD_LINK_TTL);
    crate::store::put_link_token(&state.db, account_id, &hash, &expires_at)
        .await
        .map_err(|e| format!("saving the set-password link failed: {e}"))?;
    Ok(Link {
        url: format!("{base}{SET_PASSWORD_PATH}#token={token}"),
        expires_at,
        valid_until,
    })
}

/// `401 link_invalid`: a token that is unknown, used or expired.
pub fn link_invalid() -> Refusal {
    Refusal {
        status: axum::http::StatusCode::UNAUTHORIZED,
        code: "link_invalid",
        message: "this set-password link is unknown, used or expired — ask an admin for a new \
                  one"
        .into(),
    }
}

/// The account a link's token belongs to, while it is valid: the check a
/// submit makes before it pays for a hash. Not spent here: the submit
/// spends it in the transaction that sets the password
/// (`store::set_password_by_link`).
pub async fn account_of(state: &SharedState, token: &str) -> Result<i64, Refusal> {
    let hash = crate::config::hash_api_key(token.trim());
    let Some((account_id, expires_at)) = crate::store::link_token(&state.db, &hash)
        .await
        .map_err(|e| Refusal::from(e.to_string()))?
    else {
        return Err(link_invalid());
    };
    let live =
        chrono::DateTime::parse_from_rfc3339(&expires_at).is_ok_and(|at| at > chrono::Utc::now());
    if !live {
        return Err(link_invalid());
    }
    Ok(account_id)
}
