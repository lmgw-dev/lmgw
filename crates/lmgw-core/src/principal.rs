//! One principal, one gate, one vocabulary (principals design §3).
//!
//! A credential is an `api_keys` row; a request has exactly one [`Principal`],
//! resolved once from the headers; a route needs exactly one [`Cap`]. This
//! module is the vocabulary and the resolver; `server::principal_mw` resolves
//! once at the router root and `server::require` decides, per route, whether
//! the principal it found holds what the route declared.
//!
//! The resolver is deliberately a pure function of `(headers, snapshot)`: the
//! snapshot already holds every `api_keys` row, so a lookup costs one hash and
//! a scan and never a query, and a test can drive every branch without a
//! database.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types as dto;

use crate::config::{ApiKey, ApiKeyKind, Snapshot};

/// The cookie an owner's browser carries (§3.3). Host-only, `HttpOnly`,
/// `SameSite=Strict`; accepted for `owner` rows and nothing else.
pub const SESSION_COOKIE: &str = "lmgw_session";

/// Who is making this request (§3.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Principal {
    /// No credential, or one that matched no row. Not an error by itself —
    /// what it means is the capability check's decision (§3.3). The default,
    /// so that a `RequestCtx` built before resolution claims nothing.
    #[default]
    Anonymous,
    Key {
        id: i64,
        name: String,
        kind: ApiKeyKind,
        /// `Some` for `kind = agent` only: the catalog row the token belongs
        /// to, which is what `Ledger` and `AgentSelf` scope themselves by.
        agent_id: Option<String>,
    },
}

/// What a route needs (§3.2). Five words, and every route declares exactly
/// one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Public,
    Inference,
    Ledger,
    AgentSelf,
    Admin,
}

impl Cap {
    /// How the capability names itself in a refusal.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Inference => "inference",
            Self::Ledger => "ledger",
            Self::AgentSelf => "agent-self",
            Self::Admin => "admin",
        }
    }
}

impl Principal {
    /// The principal behind one resolved row.
    pub fn from_key(key: &ApiKey) -> Self {
        Self::Key {
            id: key.id,
            name: key.name.clone(),
            kind: key.kind,
            agent_id: key.agent_id.clone(),
        }
    }

    /// `api_keys.id`, for the handlers and the log line that carry it.
    pub fn key_id(&self) -> Option<i64> {
        match self {
            Self::Anonymous => None,
            Self::Key { id, .. } => Some(*id),
        }
    }

    /// The agent this principal *is*, if any — the id `Ledger` and
    /// `AgentSelf` compare their target against.
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Key {
                kind: ApiKeyKind::Agent,
                agent_id,
                ..
            } => agent_id.as_deref(),
            _ => None,
        }
    }

    /// The §3.2 table, in code.
    ///
    /// The one conditional row is `Anonymous` × `Inference`: **Require API
    /// key** off is what makes an anonymous `/v1` call legal, and that is the
    /// whole of what the toggle has ever meant (§3.8). Anonymous never holds
    /// `Admin` in either state — the dashboard's own calls carry the cookie.
    pub fn holds(&self, cap: Cap, snap: &Snapshot) -> bool {
        match self {
            Self::Anonymous => match cap {
                Cap::Public => true,
                Cap::Inference => !snap.settings.auth_enabled,
                Cap::Ledger | Cap::AgentSelf | Cap::Admin => false,
            },
            Self::Key { kind, .. } => match kind {
                // Every enabled owner row holds everything an owner holds, and
                // the rows differ only in which one leaked (§3.1) — but
                // `Ledger` is **deliberately agent-only** (§3.2): a run is
                // written by the agent that owns it, and an owner has nothing
                // to gain from forging events into one. The layer is what says
                // so, in the one sentence a refusal can be read from
                // (`403 forbidden`, naming the capability); before this the
                // handler's `run_not_owned` said it by accident, and only for
                // a run that happened to exist.
                ApiKeyKind::Owner => cap != Cap::Ledger,
                // Scoped and allow-listed as today on `Inference`; `Ledger`
                // and `AgentSelf` are narrowed to its own runs and its own id
                // by the handlers' existing ownership checks.
                ApiKeyKind::Agent => !matches!(cap, Cap::Admin),
                ApiKeyKind::Key => matches!(cap, Cap::Public | Cap::Inference),
                // `internal` rows are attribution identities, not credentials.
                // [`resolve`] never produces one, so this arm is the second
                // backstop `policy::admit`'s refusal already is (§3.1).
                ApiKeyKind::Internal => matches!(cap, Cap::Public),
            },
        }
    }

    /// How the principal names itself in a refusal — the half of a `403` that
    /// says what *was* presented (§3.9).
    pub fn describe(&self) -> String {
        match self {
            Self::Anonymous => "no credential".into(),
            Self::Key {
                kind: ApiKeyKind::Owner,
                name,
                ..
            } => format!("an owner key ('{}')", owner_label(name)),
            Self::Key {
                kind: ApiKeyKind::Agent,
                agent_id,
                ..
            } => format!(
                "an agent token (agent '{}')",
                agent_id.as_deref().unwrap_or("?")
            ),
            Self::Key {
                kind: ApiKeyKind::Internal,
                name,
                ..
            } => format!("an internal identity ('{name}')"),
            Self::Key { name, .. } => format!("a client API key ('{name}')"),
        }
    }
}

/// `owner:self-admin` reads as `self-admin` in a sentence that already says
/// "owner key".
fn owner_label(name: &str) -> &str {
    name.strip_prefix("owner:").unwrap_or(name)
}

/// A refusal in the one shape the dashboard's `decode` parses
/// ([`dto::ApiError`]), with the status the §3.9 table gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    /// `401` — the route needs a principal and the request brought none.
    pub fn session_required() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "session_required",
            message: "open the login link printed in the process log, or use the lmgw window"
                .into(),
        }
    }

    /// `401` — the bearer matched an agent whose row is switched off. Disable
    /// is the kill switch, and it says so rather than reading as a typo.
    pub fn agent_disabled(agent_id: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "agent_disabled",
            message: format!(
                "agent '{agent_id}' is disabled, so its token authenticates nothing — enable it \
                 on the agent's page"
            ),
        }
    }

    /// `401` — the credential matched an owner row that is switched off. For
    /// `owner:self-admin` this is exactly what "the self-admin plane is
    /// closed" means now (§3.7).
    pub fn owner_key_disabled(name: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "owner_key_disabled",
            message: format!(
                "owner key '{}' is disabled — enable it on Usage → Keys",
                owner_label(name)
            ),
        }
    }

    /// `401` — the credential matched a **client** row that is switched off.
    ///
    /// Same rule as the two above, and for the same reason (§3.3): a matched
    /// disabled row is never a fall-through. Without it a key the owner had
    /// switched off read as `missing or invalid gateway API key` — the wording
    /// for a typo — and the one person who could fix it was told to check the
    /// string rather than the switch.
    pub fn key_disabled(name: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "key_disabled",
            message: format!("client key '{name}' is disabled — enable it on Usage → Keys"),
        }
    }

    /// `403` — a cookie-authenticated request that did not come from the
    /// gateway's own origin (§3.6).
    ///
    /// `what` is the header that gave it away, quoted, so the owner reading a
    /// refused dashboard call can see which page sent it.
    pub fn cross_origin_refused(what: &str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "cross_origin_refused",
            message: format!(
                "the session cookie is only honoured from this gateway's own origin, and this \
                 request came from {what}"
            ),
        }
    }

    /// `403` — a principal that does not hold the capability. Names both
    /// halves, because "forbidden" alone leaves the owner guessing which
    /// credential the request actually carried.
    pub fn forbidden(cap: Cap, who: &Principal) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "forbidden",
            message: format!(
                "this route needs {}; the request presented {}",
                cap.as_str(),
                who.describe()
            ),
        }
    }
}

/// An op-level input error in the shape every `/api` op has: `400 op_failed`,
/// which is exactly what `api::ops_result` renders from a bare `Err(String)`.
///
/// Here so that an op that answers with [`Refusal`] — because *one* of its
/// refusals carries a code of its own — does not have to restate every other
/// error it already had. `?` on a `Result<_, String>` inside such a handler
/// lands here, and the body is the one it was before.
impl From<String> for Refusal {
    fn from(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "op_failed",
            message,
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(dto::ApiError {
                code: self.code.into(),
                message: self.message,
            }),
        )
            .into_response()
    }
}

/// The `lmgw_session` value out of the `Cookie` header.
///
/// Parsed by hand rather than with a cookie crate: the header is a
/// `;`-separated list of `name=value` pairs, the value lmgw sets is 76 hex-ish
/// ASCII characters with nothing to quote or percent-encode, and reading one
/// name out of that list is the entire requirement.
pub fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim() == SESSION_COOKIE)
        .map(|(_, value)| value.trim())
}

/// The row a presented credential matches, **skipping `internal` by kind
/// before any hash is compared** (§3.1).
///
/// That order is the invariant, not an optimisation: an `internal` row's
/// `key_hash` is `''` today, but a build that ever wrote a real hash into one
/// — or an owner who edited the database — must not thereby turn an
/// attribution identity into a credential.
fn matching_row<'a>(snap: &'a Snapshot, presented: &str) -> Option<&'a ApiKey> {
    let hash = crate::config::hash_api_key(presented);
    snap.api_keys
        .iter()
        .find(|k| k.kind != ApiKeyKind::Internal && k.key_hash == hash)
}

/// Resolve the credential on a request into its principal (§3.3).
///
/// **A bearer header wins over a cookie**, and a bearer that matched nothing
/// is `Anonymous` rather than a cookie fallback: presenting a key is a
/// deliberate act, and silently authenticating such a request as the browser's
/// owner session is how a page gets more authority than it asked for.
///
/// An unmatched credential costs one hash and a scan of the key table, is
/// logged at `debug` and is **not** throttled — `policy::admit` counts only
/// matched keys, and the keyspace is 256 bits. Said out loud so that nobody
/// reads a rate limit into this that is not there.
pub fn resolve(headers: &HeaderMap, snap: &Snapshot) -> Result<Principal, Refusal> {
    if let Some(bearer) = crate::agents::token::presented(headers) {
        return from_bearer(snap, bearer);
    }
    if let Some(cookie) = session_cookie(headers) {
        return from_cookie(snap, cookie);
    }
    Ok(Principal::Anonymous)
}

/// A bearer, in any of the spellings [`token::presented`] accepts.
///
/// [`token::presented`]: crate::agents::token::presented
pub fn from_bearer(snap: &Snapshot, bearer: &str) -> Result<Principal, Refusal> {
    let Some(key) = matching_row(snap, bearer) else {
        tracing::debug!("a presented bearer matched no key row");
        return Ok(Principal::Anonymous);
    };
    if key.enabled {
        return Ok(Principal::from_key(key));
    }
    // A credential the owner switched off must say so, whichever kind it is:
    // §3.3's rule is that a **matched** disabled row is never a fall-through.
    // `internal` cannot appear here at all — [`matching_row`] skips it by kind
    // before any hash is compared — so the arm exists only to keep the match
    // total.
    match key.kind {
        ApiKeyKind::Agent => Err(Refusal::agent_disabled(
            key.agent_id.as_deref().unwrap_or_default(),
        )),
        ApiKeyKind::Owner => Err(Refusal::owner_key_disabled(&key.name)),
        ApiKeyKind::Key => Err(Refusal::key_disabled(&key.name)),
        ApiKeyKind::Internal => Ok(Principal::Anonymous),
    }
}

/// The session cookie, which is a credential **only in the owner position**
/// (§3.3): a client or agent key sitting in `lmgw_session` is not an error and
/// not an authentication, it is simply not a credential there.
fn from_cookie(snap: &Snapshot, cookie: &str) -> Result<Principal, Refusal> {
    let Some(key) = matching_row(snap, cookie) else {
        tracing::debug!("a presented session cookie matched no key row");
        return Ok(Principal::Anonymous);
    };
    if key.kind != ApiKeyKind::Owner {
        tracing::debug!(
            "the session cookie carried a '{}' key; only owner keys authenticate a cookie",
            key.kind.as_str()
        );
        return Ok(Principal::Anonymous);
    }
    if key.enabled {
        Ok(Principal::from_key(key))
    } else {
        Err(Refusal::owner_key_disabled(&key.name))
    }
}

/// The §3.6 same-origin rule, for a request whose principal came from the
/// **cookie**.
///
/// The cookie is the one credential a foreign page can make a browser attach
/// on its own, and `SameSite=Strict` covers every *cross-site* case. What it
/// does not cover is a page on the **same site but another port** —
/// `http://127.0.0.1:9999`, which any local process can serve — because a site
/// is a host, not a host and a port. This is that gap, and nothing else.
///
/// Checked against the request's **own `Host`**, not a hardcoded loopback
/// list: a gateway bound to a LAN address, opened from that LAN address, is
/// the owner's own dashboard and must pass. `http://` is the only scheme the
/// listener speaks, so it is the only one an `Origin` may carry.
///
/// A bearer-authenticated request never reaches here (see the caller): `curl`
/// sends no `Origin`, and a bearer is not something a foreign page holds.
pub fn same_origin(headers: &HeaderMap) -> Result<(), Refusal> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };

    if let Some(origin) = header("origin") {
        let host = header("host").unwrap_or_default();
        return if origin == format!("http://{host}") {
            Ok(())
        } else {
            Err(Refusal::cross_origin_refused(&format!("'{origin}'")))
        };
    }
    // No `Origin`: the browser still says what kind of navigation this was.
    // `none` is the user typing the address or opening a bookmark; anything
    // but that and `same-origin` is another page's doing.
    match header("sec-fetch-site") {
        Some(site) if site != "same-origin" && site != "none" => Err(
            Refusal::cross_origin_refused(&format!("a '{site}' context (Sec-Fetch-Site)")),
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{hash_api_key, Secret, Settings};

    fn key(id: i64, name: &str, kind: ApiKeyKind, plaintext: &str, enabled: bool) -> ApiKey {
        ApiKey {
            id,
            name: name.into(),
            key_hash: hash_api_key(plaintext),
            enabled,
            kind,
            key_plain: matches!(kind, ApiKeyKind::Agent | ApiKeyKind::Owner)
                .then(|| Secret::new(plaintext)),
            agent_id: (kind == ApiKeyKind::Agent).then(|| "board".to_string()),
            ..Default::default()
        }
    }

    /// One gateway holding one of each kind, all enabled.
    fn snap() -> Snapshot {
        Snapshot {
            api_keys: vec![
                key(1, "laptop", ApiKeyKind::Key, "lmgw-client", true),
                key(
                    2,
                    "agent:board",
                    ApiKeyKind::Agent,
                    "lmgw-agent-board",
                    true,
                ),
                key(
                    3,
                    "owner:dashboard",
                    ApiKeyKind::Owner,
                    "lmgw-owner-dash",
                    true,
                ),
                key(4, "internal:agents", ApiKeyKind::Internal, "", true),
            ],
            ..Default::default()
        }
    }

    fn with_bearer(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {value}").parse().unwrap());
        h
    }

    fn with_cookie(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "cookie",
            format!("theme=dark; {SESSION_COOKIE}={value}; tz=CET")
                .parse()
                .unwrap(),
        );
        h
    }

    #[test]
    fn a_bearer_resolves_to_the_row_that_minted_it() {
        let s = snap();
        for (token, id, kind) in [
            ("lmgw-client", 1, ApiKeyKind::Key),
            ("lmgw-agent-board", 2, ApiKeyKind::Agent),
            ("lmgw-owner-dash", 3, ApiKeyKind::Owner),
        ] {
            let p = resolve(&with_bearer(token), &s).unwrap();
            assert_eq!(p.key_id(), Some(id), "{token}");
            assert!(
                matches!(p, Principal::Key { kind: k, .. } if k == kind),
                "{token}"
            );
        }
        // And in the `x-api-key` spelling `/v1` has always accepted.
        let mut h = HeaderMap::new();
        h.insert("x-api-key", "lmgw-client".parse().unwrap());
        assert_eq!(resolve(&h, &s).unwrap().key_id(), Some(1));
    }

    #[test]
    fn the_cookie_authenticates_an_owner_and_nothing_else() {
        let s = snap();
        assert_eq!(
            resolve(&with_cookie("lmgw-owner-dash"), &s)
                .unwrap()
                .key_id(),
            Some(3)
        );
        // Not an error, not a credential in that position (§3.3).
        for other in ["lmgw-client", "lmgw-agent-board"] {
            assert_eq!(
                resolve(&with_cookie(other), &s).unwrap(),
                Principal::Anonymous,
                "{other}"
            );
        }
    }

    #[test]
    fn a_credential_that_matches_nothing_is_anonymous() {
        let s = snap();
        assert_eq!(
            resolve(&with_bearer("sk-whatever"), &s).unwrap(),
            Principal::Anonymous
        );
        assert_eq!(
            resolve(&with_cookie("lmgw-owner-stale"), &s).unwrap(),
            Principal::Anonymous
        );
        assert_eq!(
            resolve(&HeaderMap::new(), &s).unwrap(),
            Principal::Anonymous
        );
    }

    #[test]
    fn a_bearer_wins_over_a_cookie() {
        let s = snap();
        let mut h = with_cookie("lmgw-owner-dash");
        h.insert("authorization", "Bearer lmgw-client".parse().unwrap());
        assert_eq!(resolve(&h, &s).unwrap().key_id(), Some(1));

        // Including when the bearer matches nothing: falling back to the
        // cookie would hand a deliberate bad-key request the owner session.
        let mut h = with_cookie("lmgw-owner-dash");
        h.insert("authorization", "Bearer sk-nope".parse().unwrap());
        assert_eq!(resolve(&h, &s).unwrap(), Principal::Anonymous);
    }

    #[test]
    fn an_internal_row_never_authenticates_however_its_hash_is_set() {
        let mut s = snap();
        // The empty hash `internal` rows really carry: an empty bearer is not
        // reachable through `presented`, so the real test is the next one.
        assert_eq!(resolve(&with_bearer(""), &s).unwrap(), Principal::Anonymous);

        // A build — or an owner with sqlite3 — that put a usable hash on an
        // internal row must not thereby have made it a credential.
        s.api_keys[3].key_hash = hash_api_key("lmgw-internal-somehow");
        assert_eq!(
            resolve(&with_bearer("lmgw-internal-somehow"), &s).unwrap(),
            Principal::Anonymous
        );
        assert_eq!(
            resolve(&with_cookie("lmgw-internal-somehow"), &s).unwrap(),
            Principal::Anonymous
        );
    }

    #[test]
    fn a_disabled_row_names_itself_rather_than_reading_as_a_typo() {
        let mut s = snap();
        s.api_keys[1].enabled = false;
        s.api_keys[2].enabled = false;

        let e = resolve(&with_bearer("lmgw-agent-board"), &s).unwrap_err();
        assert_eq!(e.code, "agent_disabled");
        assert_eq!(e.status, StatusCode::UNAUTHORIZED);
        assert!(e.message.contains("board"), "{}", e.message);

        let e = resolve(&with_bearer("lmgw-owner-dash"), &s).unwrap_err();
        assert_eq!(e.code, "owner_key_disabled");
        assert!(e.message.contains("owner key 'dashboard'"), "{}", e.message);

        // The cookie is the same credential by another door.
        let e = resolve(&with_cookie("lmgw-owner-dash"), &s).unwrap_err();
        assert_eq!(e.code, "owner_key_disabled");

        // A disabled *client* key too: §3.3's rule is that a matched disabled
        // row never falls through, whichever of the three kinds it is.
        s.api_keys[0].enabled = false;
        let e = resolve(&with_bearer("lmgw-client"), &s).unwrap_err();
        assert_eq!(e.code, "key_disabled");
        assert_eq!(e.status, StatusCode::UNAUTHORIZED);
        assert!(e.message.contains("client key 'laptop'"), "{}", e.message);

        // A key that matches *nothing* still falls through, which is what
        // keeps `/v1`'s "any key value works" promise with the toggle off.
        assert_eq!(
            resolve(&with_bearer("lmgw-never-existed"), &s).unwrap(),
            Principal::Anonymous
        );
    }

    #[test]
    fn the_same_origin_rule_is_the_request_s_own_host() {
        fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, v.parse().unwrap());
            }
            h
        }

        // The dashboard itself, on loopback and on a LAN bind alike.
        for (origin, host) in [
            ("http://127.0.0.1:8001", "127.0.0.1:8001"),
            ("http://192.168.1.10:8001", "192.168.1.10:8001"),
        ] {
            assert!(
                same_origin(&headers(&[("origin", origin), ("host", host)])).is_ok(),
                "{origin}"
            );
        }

        // Another port on the same site is exactly the case `SameSite=Strict`
        // does not catch, and the one this rule exists for.
        for origin in [
            "http://127.0.0.1:9999",
            "http://evil.example",
            "https://127.0.0.1:8001",
        ] {
            let e = same_origin(&headers(&[("origin", origin), ("host", "127.0.0.1:8001")]))
                .unwrap_err();
            assert_eq!(e.code, "cross_origin_refused", "{origin}");
            assert_eq!(e.status, StatusCode::FORBIDDEN, "{origin}");
            assert!(e.message.contains(origin), "{}", e.message);
        }

        // No `Origin`: the fetch metadata decides, and `none` (typed address,
        // bookmark) is the owner opening their own dashboard.
        for site in ["same-origin", "none"] {
            assert!(
                same_origin(&headers(&[
                    ("sec-fetch-site", site),
                    ("host", "127.0.0.1:8001")
                ]))
                .is_ok(),
                "{site}"
            );
        }
        for site in ["cross-site", "same-site"] {
            let e = same_origin(&headers(&[
                ("sec-fetch-site", site),
                ("host", "127.0.0.1:8001"),
            ]))
            .unwrap_err();
            assert_eq!(e.code, "cross_origin_refused", "{site}");
            assert!(e.message.contains(site), "{}", e.message);
        }

        // Neither header: curl, a container, the shell's own window.
        assert!(same_origin(&HeaderMap::new()).is_ok());
    }

    #[test]
    fn the_capability_table_is_what_holds_says() {
        let open = Snapshot::default();
        let closed = Snapshot {
            settings: Settings {
                auth_enabled: true,
                ..Settings::default()
            },
            ..Default::default()
        };
        let all = [
            Cap::Public,
            Cap::Inference,
            Cap::Ledger,
            Cap::AgentSelf,
            Cap::Admin,
        ];

        let anon = Principal::Anonymous;
        for cap in all {
            assert_eq!(
                anon.holds(cap, &open),
                matches!(cap, Cap::Public | Cap::Inference),
                "anonymous / {cap:?} with the toggle off"
            );
            assert_eq!(
                anon.holds(cap, &closed),
                cap == Cap::Public,
                "anonymous / {cap:?} with the toggle on"
            );
        }

        let client = Principal::from_key(&key(1, "laptop", ApiKeyKind::Key, "c", true));
        let agent = Principal::from_key(&key(2, "agent:board", ApiKeyKind::Agent, "a", true));
        let owner = Principal::from_key(&key(3, "owner:dashboard", ApiKeyKind::Owner, "o", true));
        let internal = Principal::from_key(&key(4, "internal:x", ApiKeyKind::Internal, "", true));
        for cap in all {
            // A client is an inference credential and never more — the
            // toggle does not widen it either.
            assert_eq!(
                client.holds(cap, &closed),
                matches!(cap, Cap::Public | Cap::Inference),
                "client / {cap:?}"
            );
            assert_eq!(
                agent.holds(cap, &closed),
                cap != Cap::Admin,
                "agent / {cap:?}"
            );
            // Everything except the ledger, which is the agent's alone.
            assert_eq!(
                owner.holds(cap, &closed),
                cap != Cap::Ledger,
                "owner / {cap:?}"
            );
            assert_eq!(
                internal.holds(cap, &closed),
                cap == Cap::Public,
                "internal / {cap:?}"
            );
        }
    }

    #[test]
    fn a_refusal_names_the_capability_and_what_was_presented() {
        let agent = Principal::from_key(&key(2, "agent:board", ApiKeyKind::Agent, "a", true));
        let r = Refusal::forbidden(Cap::Admin, &agent);
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        assert_eq!(
            r.message,
            "this route needs admin; the request presented an agent token (agent 'board')"
        );
        assert_eq!(
            Refusal::forbidden(Cap::Admin, &Principal::Anonymous).message,
            "this route needs admin; the request presented no credential"
        );
    }
}
