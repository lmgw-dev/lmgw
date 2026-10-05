//! The route walk (principals design §10, Part 1).
//!
//! Two questions, both asked of the **real** router:
//!
//! 1. Does every registered route appear in `server::CAPABILITY_TABLE`, and
//!    does every row of that table answer a real route? §3.2 says "there is no
//!    route that is unlisted", and a table nobody checks is a comment.
//! 2. For each of the five principal kinds, does every row answer the status
//!    class its capability implies?
//!
//! Direction 1 is answered by reading the crate's own source: axum's `Router`
//! keeps no public list of what was registered, so the registrations are
//! parsed out of the `.route(..)` calls that made them. That is a text scan
//! and it is stated as one — but it is not a loose one: an unparsed path or a
//! wrong mount prefix produces a row that direction 2 then drives over HTTP
//! and finds missing, so the two halves check each other.

use crate::common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lmgw_core::config::hash_api_key;
use lmgw_core::principal::Cap;
use lmgw_core::server::CAPABILITY_TABLE;
use lmgw_core::state::{AppState, SharedState};
use serde_json::json;

use common::Gw;

/// The capture value every `{id}` / `{name}` / `{*rest}` in the table is asked
/// with. Deliberately an id nothing in the catalog has: the walk is about the
/// gate, and a real id would have handlers doing real work behind it.
const PROBE: &str = "route-walk-probe";

// ---------------------------------------------------------------------------
// 1. The table and the router agree
// ---------------------------------------------------------------------------

/// Which mount prefix a source file's routes are registered under.
///
/// Only one file has one: `build_router` nests its two `/v1` groups, and every
/// other `routes()` is merged at the root. The `nest` call is asserted below,
/// so this cannot quietly go stale.
fn prefix_for(file: &Path) -> &'static str {
    match file.file_name().and_then(|n| n.to_str()) {
        Some("server.rs") => "/v1",
        _ => "",
    }
}

#[test]
fn the_capability_table_lists_every_registered_route_and_nothing_else() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let server_rs = std::fs::read_to_string(src.join("server.rs")).unwrap();
    assert!(
        server_rs.contains(r#".nest("/v1", api)"#),
        "the `/v1` mount prefix this scan assumes is gone from build_router"
    );

    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    let mut registered: BTreeSet<(String, String)> = BTreeSet::new();
    for file in &files {
        registrations(file, prefix_for(file), &mut registered);
    }
    assert!(
        registered.len() > 50,
        "the source scan found only {} routes, which means it stopped working",
        registered.len()
    );

    let listed: BTreeSet<(String, String)> = CAPABILITY_TABLE
        .iter()
        .map(|(m, p, _)| ((*m).to_string(), (*p).to_string()))
        .collect();

    let missing: Vec<_> = registered.difference(&listed).collect();
    assert!(
        missing.is_empty(),
        "registered but not in CAPABILITY_TABLE — every route declares a \
         capability, and `Admin` is the one a new route gets by being listed: \
         {missing:#?}"
    );
    let extra: Vec<_> = listed.difference(&registered).collect();
    assert!(
        extra.is_empty(),
        "in CAPABILITY_TABLE but registered nowhere — a row for a route that \
         no longer exists gates nothing: {extra:#?}"
    );
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            // A `tests.rs` module builds routers of its own, which are not
            // this gateway's surface.
            && path.file_name().is_some_and(|n| n != "tests.rs")
        {
            out.push(path);
        }
    }
}

/// Every `(method, path)` one source file registers, with `mount` in front.
fn registrations(file: &Path, mount: &str, out: &mut BTreeSet<(String, String)>) {
    let text = std::fs::read_to_string(file).unwrap();
    // A `#[cfg(test)]` tail is a test's own scaffolding, not the router.
    let text = text.split("#[cfg(test)]").next().unwrap();

    let mut from = 0;
    while let Some(at) = text[from..].find(".route(") {
        let start = from + at + ".route(".len();
        let end = close_paren(text, start);
        let arg = &text[start..end];
        let path = first_string(arg).unwrap_or_else(|| {
            panic!(
                "{}: a .route(..) whose path is not a literal",
                file.display()
            )
        });
        for method in methods(arg) {
            out.insert((method, format!("{mount}{path}")));
        }
        from = end;
    }
}

/// The index just past the `)` that closes the `(` before `from`, skipping
/// string literals and line comments so a `)` inside either is not counted.
fn close_paren(text: &str, from: usize) -> usize {
    let bytes = text.as_bytes();
    let (mut depth, mut i) = (1usize, from);
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unbalanced .route( argument list");
}

fn first_string(arg: &str) -> Option<String> {
    let start = arg.find('"')? + 1;
    let end = start + arg[start..].find('"')?;
    Some(arg[start..end].to_string())
}

/// The method-router constructors named inside one `.route(..)` argument list.
/// `any(..)` answers every method and is written `*` in the table.
fn methods(arg: &str) -> Vec<String> {
    const VERBS: [&str; 8] = [
        "get", "post", "put", "delete", "patch", "head", "options", "any",
    ];
    let bytes = arg.as_bytes();
    let mut found = Vec::new();
    for verb in VERBS {
        let mut from = 0;
        while let Some(at) = arg[from..].find(verb) {
            let start = from + at;
            let end = start + verb.len();
            let before_ok = start == 0 || !is_ident(bytes[start - 1]);
            if before_ok && bytes.get(end) == Some(&b'(') {
                let name = if verb == "any" {
                    "*".to_string()
                } else {
                    verb.to_uppercase()
                };
                if !found.contains(&name) {
                    found.push(name);
                }
            }
            from = end;
        }
    }
    found
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b':'
}

// ---------------------------------------------------------------------------
// 2. Every row answers what the table implies
// ---------------------------------------------------------------------------

/// The five principals of §3.1, as a request sees them — plus the **sixth
/// caller**, which is the owner again by the other door.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Who {
    /// No credential, **Require API key** off — the default install.
    AnonOpen,
    /// No credential, **Require API key** on.
    AnonClosed,
    Client,
    Agent,
    Owner,
    /// The same owner key in the **session cookie**: what the dashboard is,
    /// and the only credential that is additionally held to §3.6. Every route
    /// has to answer it exactly as it answers the bearer — a route that reads
    /// the `Authorization` header by hand answers one and not the other, and
    /// that difference is invisible from a suite that only ever sends bearers.
    OwnerCookie,
}

/// What the §3.2 table says the gate owes each principal.
///
/// `NotRefused` is "the layer let it through": what the handler then says is
/// the handler's business, and for `Ledger` and `AgentSelf` that is a `403` of
/// its own when the run or the row belongs to another agent (§3.9's
/// `run_not_owned` / `agent_not_owned`) — which is exactly the case a probe id
/// nothing owns produces.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    /// Neither 401 nor 403: the request reached the handler.
    Admitted,
    Unauthorized,
    Forbidden,
    /// Admitted by the layer; the handler may still refuse by ownership.
    NotUnauthorized,
}

fn expected(cap: Cap, who: Who) -> Expect {
    use Expect::*;
    use Who::*;
    match cap {
        Cap::Public => Admitted,
        Cap::Inference => match who {
            AnonClosed => Unauthorized,
            _ => Admitted,
        },
        Cap::Ledger => match who {
            AnonOpen | AnonClosed => Unauthorized,
            // `Ledger` is deliberately agent-only (§3.2), so an owner is
            // refused by the *layer* exactly as a client is: a run is written
            // by the agent that owns it.
            Client | Owner | OwnerCookie => Forbidden,
            // The layer admits an agent; the handler is what says whose run
            // it is (`run_not_owned`), and a probe id nothing owns is that.
            Agent => NotUnauthorized,
        },
        Cap::AgentSelf => match who {
            AnonOpen | AnonClosed => Unauthorized,
            Client => Forbidden,
            Agent => NotUnauthorized,
            Owner | OwnerCookie => Admitted,
        },
        Cap::Admin => match who {
            AnonOpen | AnonClosed => Unauthorized,
            Client | Agent => Forbidden,
            Owner | OwnerCookie => Admitted,
        },
    }
}

impl Expect {
    fn check(self, status: u16) -> bool {
        match self {
            Self::Admitted => status != 401 && status != 403,
            Self::Unauthorized => status == 401,
            Self::Forbidden => status == 403,
            Self::NotUnauthorized => status != 401,
        }
    }
}

/// A gateway with one key of every kind, plus the owner row `init_for_tests`
/// already seeded.
struct Walk {
    gw: Gw,
    client: String,
    agent: String,
    /// One HTTP client for the whole walk. Building one loads the system's
    /// trust store, and a client per request made that the bulk of this
    /// test's run time; no credential lives in it (every request sets its
    /// own headers, and it keeps no cookie jar), so sharing it changes no
    /// answer.
    http: reqwest::Client,
}

async fn walk_gateway(auth_enabled: bool) -> Walk {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = state.snapshot().settings.clone();
    settings.auth_enabled = auth_enabled;
    lmgw_core::store::save_settings(&state.db, &settings)
        .await
        .unwrap();

    let client = "lmgw-route-walk-client".to_string();
    let agent = "lmgw-agent-route-walk".to_string();
    insert_key(&state, "walk-client", &client, "key", None).await;
    insert_key(&state, "agent:walker", &agent, "agent", Some("walker")).await;
    state.reload_snapshot().await.unwrap();

    // No redirect following: `/ui/*` answers with one, and what is being
    // asked here is what *this* route said.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    Walk {
        gw: common::serve(state).await,
        client,
        agent,
        http,
    }
}

async fn insert_key(state: &SharedState, name: &str, plain: &str, kind: &str, agent: Option<&str>) {
    // `key_plain` is present for exactly the two kinds lmgw has to hand back
    // (`agent`, `owner`) and NULL for a client key — the table's own CHECK.
    let plain_column = matches!(kind, "agent" | "owner").then(|| plain.to_string());
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, key_plain, enabled, kind, agent_id)
         VALUES (?1, ?2, ?3, 1, ?4, ?5)",
    )
    .bind(name)
    .bind(hash_api_key(plain))
    .bind(plain_column)
    .bind(kind)
    .bind(agent)
    .execute(&state.db)
    .await
    .unwrap();
}

impl Walk {
    fn bearer(&self, who: Who) -> Option<&str> {
        match who {
            Who::AnonOpen | Who::AnonClosed | Who::OwnerCookie => None,
            Who::Client => Some(&self.client),
            Who::Agent => Some(&self.agent),
            Who::Owner => Some(&self.gw.key),
        }
    }

    /// The one principal that rides in the jar instead. No `Origin` and no
    /// `Sec-Fetch-Site` on these requests, which is a `curl`, a container —
    /// and what §3.6 allows; the origin rule itself is `principal_gate.rs`'s.
    fn cookie(&self, who: Who) -> Option<&str> {
        (who == Who::OwnerCookie).then_some(self.gw.key.as_str())
    }

    async fn status(&self, method: &str, path: &str, who: Who) -> u16 {
        let url = format!("{}{}", self.gw.base, fill(path));
        let verb = if method == "*" { "GET" } else { method };
        let mut req = self.http.request(verb.parse().unwrap(), &url);
        if verb == "POST" {
            // `POST /api/session` carries its credential in the *body* (§3.4),
            // and a body with no key in it would answer every principal alike
            // for a reason that has nothing to do with the gate. Given the one
            // key this walk holds, the route answers `204` to all five — which
            // is what `Public` means and what the row claims.
            req = if path == "/api/session" {
                req.json(&json!({ "token": self.gw.key }))
            } else {
                req.json(&json!({}))
            };
        }
        // `/mcp*` answers 406 to a client that does not accept JSON, which
        // would be a refusal this walk cannot tell apart from a gate's.
        req = req.header("accept", "application/json");
        if let Some(bearer) = self.bearer(who) {
            req = req.header("authorization", format!("Bearer {bearer}"));
        }
        if let Some(session) = self.cookie(who) {
            req = req.header("cookie", format!("lmgw_session={session}"));
        }
        req.send()
            .await
            .unwrap_or_else(|e| panic!("{verb} {url} as {who:?}: {e}"))
            .status()
            .as_u16()
    }
}

/// One capture value per `{..}` segment. `{job_id}` is an `i64` in the
/// handler's own extractor, so it gets a number rather than the word.
fn fill(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if !seg.starts_with('{') {
                return seg.to_string();
            }
            let name = seg
                .trim_matches(|c| c == '{' || c == '}')
                .trim_start_matches('*');
            if name.contains("job") {
                "1".to_string()
            } else {
                PROBE.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[tokio::test]
async fn every_table_row_answers_the_status_class_its_capability_implies() {
    let open = walk_gateway(false).await;
    let closed = walk_gateway(true).await;

    for (method, path, cap) in CAPABILITY_TABLE {
        for who in [
            Who::AnonOpen,
            Who::Client,
            Who::Agent,
            Who::Owner,
            Who::OwnerCookie,
        ] {
            let status = open.status(method, path, who).await;
            let want = expected(*cap, who);
            assert!(
                want.check(status),
                "{method} {path} ({cap:?}) as {who:?}: got {status}, wanted {want:?}"
            );
            // A row in the table that no route answers would otherwise pass
            // every `Unauthorized` assertion by 404ing — except it would not,
            // because a 404 is neither 401 nor 403. Said out loud: the other
            // half of this file is what proves the route exists.
            assert_ne!(
                status, 405,
                "{method} {path}: the table names a method this route does not answer"
            );
        }
        let status = closed.status(method, path, Who::AnonClosed).await;
        let want = expected(*cap, Who::AnonClosed);
        assert!(
            want.check(status),
            "{method} {path} ({cap:?}) with Require API key on: got {status}, wanted {want:?}"
        );
    }
}
