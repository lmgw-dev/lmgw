//! Which path a `/v1/responses` request takes (openai egress design §6,
//! decision 12): forwarded natively to an upstream that implements the API,
//! or served by lmgw's own loop.
//!
//! [`Upstream::forwards_responses`](crate::config::Upstream::forwards_responses)
//! says whether the route's upstream can take it natively at all: the OpenAI
//! platform always, an `openai_compatible` row by its flag. Even there a
//! request stays in the loop when only the loop can serve it:
//! - an `{"type":"mcp"}` block whose label lmgw serves to this caller
//!   ([`serves`](crate::mcp::exec::serves)). The upstream would try to reach
//!   that server itself. A label lmgw does not serve goes native: on the
//!   platform that is OpenAI's hosted MCP tool;
//! - a `previous_response_id` held in lmgw's own response store: a chain lmgw
//!   synthesized continues where it started.
//!
//! An `mcp` block with a `connector_id`, or a `server_url` that is not lmgw's
//! own `/mcp`, names a server for the upstream to dial ([`Foreign`]): never
//! lmgw's to serve, whatever its label. It goes native with the body; a
//! request that takes the loop anyway is refused naming it, rather than
//! having lmgw's same-label server answer in its place.
//!
//! A request that needs the loop for its tools and continues a chain only the
//! upstream holds can be served by neither path, and is refused naming both.
//! So is one that needs the loop and carries what only the native path runs
//! (a hosted tool, an item reference, `truncation: "auto"`: the request's
//! [`native_only`](crate::ingress::responses::NativeOnlyParts) parts). On an
//! upstream that does not forward `/v1/responses` at all, those parts are
//! refused with the error the parser used to give them (§6.1).
//!
//! On the native path a client key may not name what the upstream holds
//! under the owner's provider key ([`super::references`]).

use crate::config::Upstream;
use crate::error::GatewayError;
use crate::ingress::responses::ResponsesRequest;
use crate::mcp::scope::ToolScope;
use crate::mcp::spec::McpToolSpec;
use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// Where a request goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Path {
    /// Forwarded to the upstream's own `/v1/responses`.
    Native,
    /// Served by lmgw's loop over the upstream's chat completions.
    Loop,
}

/// The path for `req` on `upstream`; `Err` when neither can serve it.
pub(super) async fn choose(
    state: &SharedState,
    ctx: &RequestCtx,
    upstream: &Upstream,
    req: &ResponsesRequest,
    body: &serde_json::Value,
) -> Result<Path, GatewayError> {
    let bind_addr = state.snapshot().settings.bind_addr.clone();
    let foreign: Vec<Foreign> = req
        .mcp_tools
        .iter()
        .filter_map(|spec| Foreign::of(spec, ctx.host.as_deref(), &bind_addr))
        .collect();
    if !upstream.forwards_responses() {
        // What only a native upstream serves is refused with the error the
        // parser gave it before it was accepted there (§6.1).
        if let Some(n) = req.native_only.first() {
            return Err(n.error.clone());
        }
        let why = format!("'{}' does not take /v1/responses natively", upstream.name);
        return in_the_loop(&foreign, req, upstream, || why);
    }
    // Native, the body goes to the upstream as sent, and a client key must
    // not reach another key's conversation through it. lmgw creates no
    // conversations and no prompt templates, so a `conversation` or a
    // `prompt` is never a client key's own: it answers as an unknown id. The
    // owner's passes (ids made outside lmgw on the same provider key).
    // Items, files, vector stores, containers and skills the body names are
    // scoped the same way below ([`super::references`]).
    let reader = super::reader(ctx);
    if reader != crate::store::ResponseReader::Owner {
        for field in ["conversation", "prompt"] {
            if let Some(v) = body.get(field).filter(|c| !c.is_null()) {
                let id = v
                    .as_str()
                    .or_else(|| v.get("id").and_then(serde_json::Value::as_str))
                    .unwrap_or_default();
                return Err(super::no_stored_response(id));
            }
        }
    }
    let served = served_labels(state, ctx, req).await;
    let ours = match req.previous_response_id.as_deref() {
        None => false,
        // Another key's response counts as one lmgw does not hold.
        Some(prev) => crate::store::has_response(&state.db, prev, super::reader(ctx))
            .await
            .map_err(|e| {
                GatewayError::Internal(format!("looking up stored response {prev}: {e}"))
            })?,
    };
    // A native id lmgw recorded under another key is as unknown to this
    // caller as a stored one: only the owner may continue ids lmgw never saw.
    if let (Some(prev), false) = (req.previous_response_id.as_deref(), ours) {
        let native_known = crate::store::get_native_response(&state.db, prev, reader)
            .await
            .map_err(|e| GatewayError::Internal(format!("looking up native response {prev}: {e}")))?
            .is_some();
        if !native_known && reader != crate::store::ResponseReader::Owner {
            return Err(super::no_stored_response(prev));
        }
    }
    // What the upstream holds under the owner's provider key: never a file,
    // a vector store, a container or a skill (lmgw proxies none of their
    // APIs, so no client key made one). Answered as an unknown id.
    if reader != crate::store::ResponseReader::Owner {
        if let Some(id) = super::references::first_foreign(body) {
            return Err(super::no_stored_response(&id));
        }
    }
    let path = decide(req, upstream, &foreign, ours, &served)?;
    // A stored item the input names by id is resolved by the upstream, so
    // a client key may name only the items of native answers it was given
    // (migration 0093). Only on the native path: the loop resolves no id.
    if path == Path::Native && reader != crate::store::ResponseReader::Owner {
        // Encrypted reasoning without the id that names it: nothing vouches
        // for it, and OpenAI requires the id (WP8 verify V-N1).
        if let Some(at) = super::seals::first_without_id(body) {
            return Err(GatewayError::BadRequest(format!(
                "input[{at}] is a reasoning item with encrypted_content but no id; a reasoning \
                 input item needs the id it was answered with — send the item back as it came"
            )));
        }
        let named = super::references::named_items(body);
        if let Some(id) = crate::store::first_item_not_of(&state.db, &named, ctx.principal.key_id())
            .await
            .map_err(|e| GatewayError::Internal(format!("looking up input items: {e}")))?
        {
            return Err(super::no_stored_response(&id));
        }
    }
    Ok(path)
}

/// [`choose`]'s decision once the ids are scoped: `ours` is whether the
/// `previous_response_id` is in lmgw's own store, `served` the `mcp` labels
/// lmgw serves this caller.
fn decide(
    req: &ResponsesRequest,
    upstream: &Upstream,
    foreign: &[Foreign],
    ours: bool,
    served: &[String],
) -> Result<Path, GatewayError> {
    let labels = || {
        served
            .iter()
            .map(|l| format!("'{l}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (req.previous_response_id.as_deref(), ours, served.is_empty()) {
        (None, _, true) | (Some(_), false, true) => Ok(Path::Native),
        (Some(prev), false, false) => Err(GatewayError::BadRequest(format!(
            "this request names mcp tools lmgw serves ({}), which only lmgw's own loop can run, \
             and continues previous_response_id '{prev}', which lmgw does not hold, so only \
             '{}' can continue it — send the conversation in 'input' instead of \
             previous_response_id, or leave out the mcp tools lmgw serves",
            labels(),
            upstream.name,
        ))),
        (Some(prev), true, _) => in_the_loop(foreign, req, upstream, || {
            format!("it continues previous_response_id '{prev}', which lmgw's own loop stored")
        }),
        (None, _, false) => in_the_loop(foreign, req, upstream, || {
            format!(
                "it names mcp tools lmgw serves ({}), which only lmgw's own loop can run",
                labels()
            )
        }),
    }
}

/// [`Path::Loop`], unless something only an upstream can serve is in the
/// way: `why` says why the request is in the loop. On an upstream that
/// forwards `/v1/responses`, a request that needs the loop and also carries
/// what only the native path runs (a hosted tool, an item reference, …) can
/// be served by neither, and is refused naming both.
fn in_the_loop(
    foreign: &[Foreign],
    req: &ResponsesRequest,
    upstream: &Upstream,
    why: impl FnOnce() -> String,
) -> Result<Path, GatewayError> {
    if !req.native_only.is_empty() && upstream.forwards_responses() {
        return Err(GatewayError::BadRequest(format!(
            "this request needs lmgw's own loop because {}, and it also uses {}, which only \
             '{}' runs natively and lmgw's loop cannot, so neither can serve it — leave out \
             one or the other",
            why(),
            req.native_only.named(),
            upstream.name,
        )));
    }
    let Some(first) = foreign.first() else {
        return Ok(Path::Loop);
    };
    let named = foreign
        .iter()
        .map(Foreign::describe)
        .collect::<Vec<_>>()
        .join("; ");
    Err(GatewayError::BadRequest(format!(
        "{named}: lmgw does not dial a server a client names, so only an upstream that runs \
         hosted MCP tools itself can serve {}, and this request is served by lmgw's own loop \
         because {} — send it to an alias whose upstream takes it natively, or name the \
         server by one of lmgw's own labels without {}",
        if foreign.len() == 1 { "it" } else { "them" },
        why(),
        first.field,
    )))
}

/// An `mcp` block that names a server for the upstream to dial.
struct Foreign {
    label: String,
    /// `server_url` or `connector_id`.
    field: &'static str,
}

impl Foreign {
    /// `spec` when it is one: a `connector_id`, or a `server_url` that is not
    /// lmgw's own `/mcp` ([`own_mcp_url`]).
    fn of(spec: &McpToolSpec, host: Option<&str>, bind_addr: &str) -> Option<Self> {
        let field = if spec.connector_id.is_some() {
            "connector_id"
        } else if spec
            .server_url
            .as_deref()
            .is_some_and(|url| !own_mcp_url(url, host, bind_addr))
        {
            "server_url"
        } else {
            return None;
        };
        Some(Self {
            label: spec.server_label.clone(),
            field,
        })
    }

    fn describe(&self) -> String {
        match self.field {
            "connector_id" => format!("mcp tool '{}' names a connector_id", self.label),
            _ => format!(
                "mcp tool '{}' names a server_url that is not this gateway's /mcp",
                self.label
            ),
        }
    }
}

/// Whether `url` is lmgw's own `/mcp`: an http(s) URL whose path is `/mcp`,
/// on the authority the request was sent to (`host`), or on one of the bind
/// address's own host names (`localhost` included) at its port.
fn own_mcp_url(url: &str, host: Option<&str>, bind_addr: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") || u.path().trim_end_matches('/') != "/mcp" {
        return false;
    }
    let Some(h) = u.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    let authority = match u.port() {
        Some(p) => format!("{h}:{p}"),
        None => h.clone(),
    };
    if host.is_some_and(|sent_to| sent_to.eq_ignore_ascii_case(&authority)) {
        return true;
    }
    let bind_port = bind_addr
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok());
    u.port_or_known_default()
        .is_some_and(|p| Some(p) == bind_port)
        && (h == "localhost" || crate::net::own_host_names(bind_addr).contains(&h))
}

/// The request's `mcp` labels lmgw serves to its caller, in request order:
/// never a block that names a server for the upstream ([`Foreign`]).
async fn served_labels(
    state: &SharedState,
    ctx: &RequestCtx,
    req: &ResponsesRequest,
) -> Vec<String> {
    if req.mcp_tools.is_empty() {
        return Vec::new();
    }
    let bind_addr = state.snapshot().settings.bind_addr.clone();
    let ours: Vec<McpToolSpec> = req
        .mcp_tools
        .iter()
        .filter(|spec| Foreign::of(spec, ctx.host.as_deref(), &bind_addr).is_none())
        .cloned()
        .collect();
    let scope = ToolScope::of_request(state, ctx).await;
    let served = crate::mcp::exec::serves(state, &ours, &scope).await;
    ours.into_iter()
        .zip(served)
        .filter(|(_, served)| *served)
        .map(|(spec, _)| spec.server_label)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lmgw_s_own_mcp_is_the_host_it_was_sent_to_or_its_bind_address() {
        let bind = "127.0.0.1:8001";
        let own = |url| own_mcp_url(url, Some("gw.example:9000"), bind);
        assert!(own("http://gw.example:9000/mcp"));
        assert!(own("http://GW.example:9000/mcp/"));
        assert!(own("http://127.0.0.1:8001/mcp"));
        assert!(own("http://localhost:8001/mcp"));
        assert!(!own("http://gw.example:9000/mcp/admin"), "another route");
        assert!(!own("http://localhost:8002/mcp"), "another port");
        assert!(!own("https://mcp.example/mcp"));
        assert!(!own("not a url"));
    }
}
