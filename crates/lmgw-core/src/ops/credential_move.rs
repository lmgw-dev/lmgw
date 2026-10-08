//! A stored credential follows its host (client-apps design L5's note, the
//! branch review's G-3, and its verification's V-1 and V-2).
//!
//! A self-admin tool's write that moves an upstream's `base_url`, or an MCP
//! server's `url`, to another address while the row holds a credential must
//! restate the credential in the same call, or nothing is written: one call
//! must not send the owner's provider key, or a server's `Authorization:`
//! header, to a host the call chose. The check sits where the move is
//! applied — the merged row of `upstream_set`'s and `mcp_server_set`'s
//! `update | enable | disable` arm — so every action that can move a row
//! meets it, one added to that arm included, and it compares the row as
//! stored with the row about to be written.
//!
//! The dashboard's own ops (`/api/op/*`, the owner's credential) move a row
//! as asked: [`RowWriter::Dashboard`].

use crate::config::{McpServer, Upstream};

/// Who writes an upstream or an MCP server row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowWriter {
    /// The dashboard's own op (`/api/op/*`), with the owner's credential.
    Dashboard,
    /// A self-admin tool (`lmgw__upstream_set`, `lmgw__mcp_server_set`),
    /// whoever calls it: a device's turn, an agent's run, Admin Chat, a model
    /// on `/mcp/admin`.
    Tool,
}

/// Why `writer` may not move upstream `cur` to `base_url` with `api_key` as
/// the call's own key (`None`: the call sends none, and the stored one would
/// stay): the row holds a key the call does not restate, or extra headers,
/// which no tool sets. `None` when nothing moves or nothing would travel.
pub(super) fn upstream_refusal(
    writer: RowWriter,
    cur: &Upstream,
    base_url: &str,
    api_key: Option<&str>,
) -> Option<String> {
    if writer == RowWriter::Dashboard || same_address(base_url, &cur.base_url) {
        return None;
    }
    if !cur.extra_headers.is_empty() {
        return Some(format!(
            "upstream '{}' sends extra headers, which only the dashboard sets: moving it to \
             {base_url} is the dashboard's, where its headers are set for the new address. \
             Nothing was changed",
            cur.name
        ));
    }
    let holds_key = cur.api_key.as_deref().is_some_and(|k| !k.is_empty());
    let restated = api_key.is_some_and(|k| !k.trim().is_empty());
    (holds_key && !restated).then(|| {
        format!(
            "upstream '{}' holds an API key: moving it to {base_url} needs the key for that \
             address in the same call (api_key), so the stored one is never sent to a host it \
             was not given for. Nothing was changed",
            cur.name
        )
    })
}

/// Why `writer` may not move MCP server `cur` to `url` when `headers` is the
/// call's own `headers` text (`None`: absent or `null`, and the stored ones
/// would stay; `""` sends none): the row holds headers the call does not
/// restate. `None` when nothing moves or nothing would travel.
pub(super) fn mcp_server_refusal(
    writer: RowWriter,
    cur: &McpServer,
    url: Option<&str>,
    headers: Option<&str>,
) -> Option<String> {
    let to = url?;
    let moved = cur.url.as_deref().is_none_or(|was| !same_address(to, was));
    if writer == RowWriter::Dashboard || !moved {
        return None;
    }
    (!cur.headers.is_empty() && headers.is_none()).then(|| {
        format!(
            "MCP server '{}' sends headers, which may hold its credential: moving it to {to} \
             needs the headers for that address in the same call (headers, as text; \"\" sends \
             none), so the stored ones are never sent to a host they were not given for. \
             Nothing was changed",
            cur.name
        )
    })
}

/// Whether two addresses are the same one, as written: a trailing `/` and
/// the case of the scheme and host aside.
fn same_address(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches('/').to_ascii_lowercase();
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The refusals themselves are tested through the tool plane
    // (`tests/it/mcp_selfadmin.rs`), every action that moves a row.
    #[test]
    fn an_address_is_the_same_without_its_trailing_slash_or_case() {
        assert!(same_address(
            "https://API.example.com/v1/",
            "https://api.example.com/v1"
        ));
        assert!(!same_address(
            "https://x.example/v1",
            "https://api.example.com/v1"
        ));
    }
}
