//! Accounts on the web plane (headless server design §5.5, §5.6, §5.11):
//! the account ops' dispatcher, behind `POST /api/op/{name}` like every
//! op, and the set-password link's own route, `POST /api/account/password`,
//! which a browser without a session posts.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::accounts::ops::{self, Transport};
use crate::accounts::switch::{self, Cookie};
use crate::principal::{same_origin, Cap, Refusal};
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store::LinkSpent;
use crate::web::session::{self, ConnInfo};

pub fn routes(state: &SharedState) -> Router<SharedState> {
    Router::new()
        .route("/api/account/password", post(set_password))
        .layer(axum::extract::DefaultBodyLimit::max(
            session::SESSION_BODY_LIMIT,
        ))
        // `Public`: the link's token is the credential (§5.5).
        .route_layer(crate::server::require(state, Cap::Public))
}

/// `POST /api/account/password { token, password, login? }` (§5.5).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LinkPassword {
    /// The token from the link's fragment (`#token=…`).
    #[schemars(transform = lmgw_api_types::openapi_ext::secret)]
    token: String,
    /// The new password: at least 12 characters, up to 2 MiB.
    #[schemars(transform = lmgw_api_types::openapi_ext::secret)]
    password: String,
    /// Account 1's sign-in name, with its first password only (§5.3).
    #[serde(default)]
    login: Option<String>,
}

/// The link's submit: the token checked by its hash, the transport rule
/// (D26) and the password rules, then, in one write transaction, the token
/// spent and the password set (so of two submits of one link exactly one
/// does it); then the account's sessions ended and this browser signed in.
/// A link of a disabled account is refused and kept.
async fn set_password(
    State(state): State<SharedState>,
    Extension(ctx): Extension<RequestCtx>,
    conn: ConnInfo,
    headers: HeaderMap,
    Json(body): Json<LinkPassword>,
) -> Response {
    let scheme = ctx.scheme;
    let result = async {
        same_origin(&headers, scheme)?;
        if !state.snapshot().accounts_on {
            return Err(crate::accounts::accounts_off());
        }
        ops::require_secure(transport(&ctx, &conn))?;
        // Checked before the hash is paid for; spent only below.
        let id = crate::accounts::links::account_of(&state, &body.token).await?;
        let snap = state.snapshot();
        let account = snap
            .account(id)
            .cloned()
            .ok_or_else(crate::accounts::links::link_invalid)?;
        if !account.enabled {
            return Err(crate::accounts::OwnerBlock::Disabled(account.login.clone()).refusal());
        }
        let login = ops::first_login(&snap, &account, body.login.as_deref())?;
        let hash = ops::hash_password(&state, &body.password).await?;
        let token_hash = crate::config::hash_api_key(body.token.trim());
        match crate::store::set_password_by_link(&state.db, &token_hash, &hash, login.as_deref())
            .await
            .map_err(|e| Refusal::from(e.to_string()))?
        {
            LinkSpent::Set(_) => {}
            LinkSpent::Invalid => return Err(crate::accounts::links::link_invalid()),
            LinkSpent::Disabled(login) => {
                return Err(crate::accounts::OwnerBlock::Disabled(login).refusal())
            }
        }
        state
            .reload_snapshot()
            .await
            .map_err(|e| Refusal::from(e.to_string()))?;
        crate::accounts::sessions::end(
            &state,
            crate::accounts::sessions::Which::Account {
                account_id: id,
                except: None,
            },
        )
        .await?;
        tracing::info!(
            "accounts: '{}' set its password with a set-password link",
            login.as_deref().unwrap_or(&account.login)
        );
        Ok((id, hash))
    }
    .await;
    match result {
        Ok((id, hash)) => {
            let client = session::client_of(&conn, &headers);
            let snap = state.snapshot();
            let actor = crate::audit::by_link(&snap, id, client.0.clone());
            let target = crate::audit::sign_in_target(&snap, Some(id), "set-password link");
            crate::audit::record_detached(&state, actor.entry("password set", target, 204));
            let start = session::SessionStart {
                account: id,
                proof: crate::accounts::sessions::Proof::Password(&hash),
                carried: ctx.principal.session(),
                with: "set-password link",
            };
            session::with_session(
                &state,
                StatusCode::NO_CONTENT.into_response(),
                start,
                scheme,
                client,
            )
            .await
        }
        Err(refusal) => {
            let snap = state.snapshot();
            if snap.accounts_on {
                let target = crate::audit::sign_in_target(&snap, None, "set-password link");
                let actor = crate::audit::anonymous(session::client_of(&conn, &headers).0);
                let entry = actor.entry("password set failed", target, refusal.status.as_u16());
                crate::audit::record_detached(&state, entry);
            }
            refusal.into_response()
        }
    }
}

fn transport(ctx: &RequestCtx, conn: &ConnInfo) -> Transport {
    Transport {
        scheme: ctx.scheme,
        direct_loopback: conn.as_ref().is_some_and(|c| c.0 .0.is_direct_loopback()),
    }
}

fn id(args: &Value, field: &str) -> Result<i64, Refusal> {
    args.get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| Refusal::from(format!("missing integer '{field}'")))
}

fn args_of<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, Refusal> {
    serde_json::from_value(args).map_err(|e| Refusal::from(e.to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    login: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Set {
    id: i64,
    display_name: Option<String>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct End {
    id: Option<i64>,
    account: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Switch {
    enabled: bool,
}

fn json<T: serde::Serialize>(v: Result<T, Refusal>) -> Result<Response, Refusal> {
    v.map(|v| Json(v).into_response())
}

/// The account ops (§5.6), from `POST /api/op/{name}`: every one but
/// `accounts_switch` answers `409 accounts_off` while accounts are off.
pub async fn account_op(
    st: &SharedState,
    ctx: &RequestCtx,
    conn: &ConnInfo,
    headers: &HeaderMap,
    name: &str,
    args: Value,
) -> Response {
    let result = async {
        // The switch, and the log a desktop keeps while accounts are off
        // (§5.11), answer either way.
        if !matches!(name, "accounts_switch" | "audit_list") && !st.snapshot().accounts_on {
            return Err(crate::accounts::accounts_off());
        }
        let who = &ctx.principal;
        match name {
            "account_list" => json(ops::list(st, who).await),
            "account_create" => {
                let a: Create = args_of(args)?;
                json(ops::create(st, who, &a.login, &a.display_name).await)
            }
            "account_set" => {
                let a: Set = args_of(args)?;
                json(ops::set(st, who, a.id, a.display_name.as_deref(), a.enabled).await)
            }
            "account_delete" => json(ops::delete(st, who, id(&args, "id")?).await),
            "account_password_link" => json(ops::password_link(st, who, id(&args, "id")?).await),
            "account_password_set" => {
                let a: ops::PasswordSet = args_of(args)?;
                // Its 429 and 503 carry `Retry-After` (§5.2).
                Ok(
                    match ops::password_set(st, who, transport(ctx, conn), a).await {
                        Ok(done) => Json(done).into_response(),
                        Err(refused) => refused.into_response(),
                    },
                )
            }
            "account_sessions" => json(ops::sessions(st, who, id(&args, "id")?).await),
            "session_end" => {
                let a: End = args_of(args)?;
                json(ops::session_end(st, a.id, a.account).await)
            }
            "account_login_link" => json(ops::login_link(st, who, id(&args, "id")?).await),
            "accounts_switch" => {
                let a: Switch = args_of(args)?;
                let (peer, ua) = session::client_of(conn, headers);
                let done = switch::switch(st, who, a.enabled, &peer, &ua).await?;
                let resp = Json(done.done).into_response();
                Ok(match done.cookie {
                    Cookie::Session(v) => {
                        session::with_cookie(resp, session::session_set_cookie(st, &v, ctx.scheme))
                    }
                    Cookie::DoorKey(k) => {
                        session::with_cookie(resp, session::door_key_cookie(&k, ctx.scheme))
                    }
                    Cookie::Clear => session::with_cookie(resp, session::clear_cookie(ctx.scheme)),
                    Cookie::Keep => resp,
                })
            }
            "audit_list" => json(crate::audit::list::list(st, args_of(args)?).await),
            other => Err(Refusal::from(format!("unknown op '{other}'"))),
        }
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}
