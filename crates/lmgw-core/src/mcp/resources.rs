//! `/mcp` passes MCP resources through, and carries the MCP Apps metadata
//! (client-apps design §7.2–§7.4, L14).
//!
//! **The MCP Apps revision followed** (§7.4): SEP-1865, "MCP Apps:
//! Interactive User Interfaces for MCP", `modelcontextprotocol/ext-apps`
//! `specification/2026-01-26/apps.mdx`, status *Stable (2026-01-26)*,
//! re-read 2026-10-09; the repository's `specification/` holds that
//! revision and `draft`, nothing newer. What lmgw relies on from it:
//! - **The extension** `io.modelcontextprotocol/ui` ([`UI_EXTENSION`]),
//!   negotiated through SEP-1724's `capabilities.extensions`, with one
//!   setting, `mimeTypes` (required). The revision defines it as the
//!   **host's** (the client's) capability, and says servers SHOULD check it
//!   before registering UI tools. lmgw is the client of every southbound
//!   server, so its own `initialize` advertises it ([`handler`]): a server
//!   built on the SDK's graceful degradation otherwise offers lmgw, and every
//!   host behind it, no UI at all. `/mcp` lists it among its server
//!   capabilities as well (SEP-1724 lets either side declare an extension):
//!   the metadata passes through.
//! - **A UI resource**: a `ui://` URI, `text/html;profile=mcp-app`
//!   ([`UI_MIME_TYPE`]), `text` or base64 `blob`. A server MAY leave it out
//!   of `resources/list`, since hosts find it through tool metadata, so a
//!   read routes by the URI and the tools that name it ([`route`]).
//! - **A tool's `_meta.ui`**: `resourceUri`, and the deprecated flat
//!   `_meta["ui/resourceUri"]` (to be removed before GA), both namespaced as
//!   the resource is ([`apps`]); `visibility`, `["model"]`, `["app"]` or
//!   both, both when absent. A host MUST NOT give a model a tool whose
//!   visibility lacks `"model"` ([`model_visible`], applied where a run
//!   lists tools for a model, and on `/mcp` for a session whose
//!   `initialize` did not declare the extension: a client that is no apps
//!   host has no views to call them for). It MUST refuse a view's call of a
//!   tool whose visibility lacks `"app"`, and a view's call of another
//!   server's app-only tool: the host bridging the view does that — `/mcp`
//!   lists an apps host every tool with its `_meta` so it can.
//! - **A resource's `_meta.ui`** (`csp` domains, `permissions`, `domain`,
//!   `prefersBorder`): passed verbatim; the host enforces it.
//!
//! **Namespacing (L14).** A server with tool prefix `p` has the authority of
//! its resource URIs prefixed: `ui://weather/card` is `ui://p__weather/card`
//! on `/mcp`, in `resources/list`, `resources/templates/list`,
//! `resources/read` (its answer's `contents[].uri` too), a tool's
//! `_meta.ui.resourceUri` in `tools/list`, and the resource links and
//! embedded resources of a tool's result ([`uri`]). A server without a prefix
//! keeps its URIs; of two that claim the same one, the stronger claim has
//! it — a tool of its own naming it, then a listing, then a template — and
//! the first by server name within the same kind (tool names collide
//! differently: [`super::names`]) ([`route`]). A URI with no authority (`urn:…`, `data:…`) keeps its
//! spelling whatever the prefix and is routed like a bare server's; a
//! prefixed server's template must have a literal `scheme://`, or it would
//! claim every such URI.
//!
//! **Reach.** A server's resources are listed to, and read by, a caller that
//! reaches the server ([`route::reaches`]): the scope's own answer for the
//! row (L16 for a device row), and — for a scope that narrows it — a tool
//! of that server the scope admits, or its whole namespace. A server is
//! asked, or woken, only for a caller its scope lets reach it.
//!
//! Children: [`uri`] (the rewrite and template matching), [`apps`] (the
//! tool metadata and results), [`route`] (the aggregate, the routing of a
//! read, reach).

pub(crate) mod apps;
mod route;
pub(crate) mod uri;

pub use apps::{model_visible, ui_resource, UI_EXTENSION, UI_MIME_TYPE};
pub use route::ResourceError;

use tokio::sync::broadcast;

use super::McpManager;

/// Northbound `notifications/resources/list_changed`: a pure "re-list now"
/// nudge, as `tools_changed` is (the same visible buffer, the same
/// catch-up on lag).
pub(crate) fn changed_channel() -> broadcast::Sender<()> {
    broadcast::channel(super::TOOLS_CHANGED_BUFFER).0
}

impl McpManager {
    /// Subscribe to the northbound `resources/list_changed` signal: an
    /// upstream's own `notifications/resources/list_changed`. A change of
    /// the aggregate's composition (a server connecting or going) is the
    /// tools signal, which `GET /mcp` reads as both.
    pub fn subscribe_resources_changed(&self) -> broadcast::Receiver<()> {
        self.resources_changed.subscribe()
    }

    /// An upstream said its resources changed.
    pub(crate) fn on_upstream_resources_changed(&self) {
        let _ = self.resources_changed.send(());
    }
}
