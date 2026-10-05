//! What `CAPABILITY_TABLE` lists that this document deliberately does not
//! document (api-docs design §4.2) — the coverage rule's third bucket, next
//! to "a `DocRoute`" and "the `POST /api/op/{name}` row".
//!
//! `tests/it/openapi_coverage.rs`'s `exclusions_are_real_rows_with_reasons`
//! (WP8) is what proves every entry here still names a real
//! `CAPABILITY_TABLE` row.
//!
//! **Owner decision, 2026-09-28.** The three internal mini-APIs — Chat,
//! Audio lab, Image lab — moved here from `planes/labs.rs` (deleted): the
//! dashboard's own backend is not a contract, so its shapes are excluded
//! entirely rather than documented `x-lmgw-internal`. `Group::Internal`, the
//! `x-lmgw-internal` derivation and constant, and the UI's "internal" chip
//! went with it — see the spec's §12.

/// `(method, path, reason)` — `method` is `CAPABILITY_TABLE`'s own spelling,
/// `*` included. The reason is what `x-lmgw-undocumented` carries.
pub(crate) const UNDOCUMENTED: &[(&str, &str, &str)] = &[
    (
        "*",
        "/agents/{id}/app",
        "The old mount of an agent's app: redirects to the agent's own origin. The app is the \
         agent's, not lmgw's API.",
    ),
    (
        "*",
        "/agents/{id}/app/",
        "The old mount of an agent's app: redirects to the agent's own origin. The app is the \
         agent's, not lmgw's API.",
    ),
    (
        "*",
        "/agents/{id}/app/{*rest}",
        "The old mount of an agent's app: redirects to the agent's own origin. The app is the \
         agent's, not lmgw's API.",
    ),
    (
        "*",
        "/agents/{id}/mcp",
        "An agent container's own MCP face, reverse-proxied. Its surface is the agent's (see \
         its manifest).",
    ),
    (
        "*",
        "/agents/{id}/mcp/{*rest}",
        "An agent container's own MCP face, reverse-proxied. Its surface is the agent's (see \
         its manifest).",
    ),
    ("GET", "/", "The dashboard SPA and its bundle."),
    ("GET", "/{*path}", "The dashboard SPA and its bundle."),
    ("GET", "/ui", "Legacy redirects to the SPA."),
    ("GET", "/ui/", "Legacy redirects to the SPA."),
    ("GET", "/ui/{*path}", "Legacy redirects to the SPA."),
    // The three internal mini-APIs (Chat, Audio lab, Image lab) — owner
    // decision 2026-09-28 (api-docs design §1, §3, §4.2 amendment): the
    // dashboard's own backend is not a contract, so it comes out of the
    // description entirely rather than being documented `x-lmgw-internal`.
    ("GET", "/chat/api/threads", DASHBOARD_BACKEND),
    ("POST", "/chat/api/threads", DASHBOARD_BACKEND),
    ("GET", "/chat/api/threads/{id}", DASHBOARD_BACKEND),
    ("POST", "/chat/api/threads/{id}/settings", DASHBOARD_BACKEND),
    ("POST", "/chat/api/threads/{id}/delete", DASHBOARD_BACKEND),
    ("POST", "/chat/api/threads/{id}/send", DASHBOARD_BACKEND),
    (
        "POST",
        "/chat/api/threads/{id}/voice/warm",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/transcribe",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/speech/stop",
        DASHBOARD_BACKEND,
    ),
    ("POST", "/chat/api/threads/{id}/pin", DASHBOARD_BACKEND),
    ("POST", "/chat/api/threads/{id}/move", DASHBOARD_BACKEND),
    ("GET", "/chat/api/search", DASHBOARD_BACKEND),
    ("GET", "/chat/api/threads/{id}/export", DASHBOARD_BACKEND),
    ("GET", "/chat/api/folders/{id}/export", DASHBOARD_BACKEND),
    ("GET", "/chat/api/export", DASHBOARD_BACKEND),
    ("GET", "/chat/api/folders", DASHBOARD_BACKEND),
    ("POST", "/chat/api/folders", DASHBOARD_BACKEND),
    ("POST", "/chat/api/folders/{id}", DASHBOARD_BACKEND),
    ("POST", "/chat/api/folders/{id}/delete", DASHBOARD_BACKEND),
    ("POST", "/chat/api/threads/{id}/archive", DASHBOARD_BACKEND),
    ("POST", "/chat/api/threads/{id}/persist", DASHBOARD_BACKEND),
    ("POST", "/chat/api/threads/{id}/continue", DASHBOARD_BACKEND),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/delete",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/edit",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/speak",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/messages/{mid}/regenerate",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/chat/api/threads/{id}/attachments",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/chat/api/attachments/{id}/delete",
        DASHBOARD_BACKEND,
    ),
    ("GET", "/chat/api/attachments/{id}", DASHBOARD_BACKEND),
    ("POST", "/chat/api/attachments/{id}/mode", DASHBOARD_BACKEND),
    (
        "POST",
        "/chat/api/attachments/{id}/transcribe",
        DASHBOARD_BACKEND,
    ),
    ("GET", "/chat/api/attachments/{id}/text", DASHBOARD_BACKEND),
    ("GET", "/audio-lab/api/models", DASHBOARD_BACKEND),
    ("GET", "/audio-lab/api/voices", DASHBOARD_BACKEND),
    ("GET", "/audio-lab/api/refs", DASHBOARD_BACKEND),
    ("POST", "/audio-lab/api/refs", DASHBOARD_BACKEND),
    ("GET", "/audio-lab/api/refs/{name}", DASHBOARD_BACKEND),
    (
        "POST",
        "/audio-lab/api/refs/{name}/delete",
        DASHBOARD_BACKEND,
    ),
    ("POST", "/audio-lab/api/refs/{name}/text", DASHBOARD_BACKEND),
    (
        "POST",
        "/audio-lab/api/refs/{name}/transcribe",
        DASHBOARD_BACKEND,
    ),
    ("POST", "/audio-lab/api/speech", DASHBOARD_BACKEND),
    ("POST", "/audio-lab/api/transcriptions", DASHBOARD_BACKEND),
    ("POST", "/audio-lab/api/alignments", DASHBOARD_BACKEND),
    ("POST", "/audio-lab/api/tasks/run", DASHBOARD_BACKEND),
    ("GET", "/image-lab/api/models", DASHBOARD_BACKEND),
    ("POST", "/image-lab/api/generate", DASHBOARD_BACKEND),
    ("POST", "/image-lab/api/edit", DASHBOARD_BACKEND),
    // The Knowledge page's backend (chat-complete design §9.5).
    ("GET", "/api/knowledge/bases", DASHBOARD_BACKEND),
    ("POST", "/api/knowledge/bases", DASHBOARD_BACKEND),
    ("GET", "/api/knowledge/bases/{id}", DASHBOARD_BACKEND),
    (
        "POST",
        "/api/knowledge/bases/{id}/settings",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/api/knowledge/bases/{id}/delete",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/api/knowledge/bases/{id}/resume",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/api/knowledge/bases/{id}/cancel",
        DASHBOARD_BACKEND,
    ),
    ("GET", "/api/knowledge/bases/{id}/files", DASHBOARD_BACKEND),
    ("POST", "/api/knowledge/bases/{id}/files", DASHBOARD_BACKEND),
    (
        "POST",
        "/api/knowledge/files/{id}/delete",
        DASHBOARD_BACKEND,
    ),
    (
        "POST",
        "/api/knowledge/files/{id}/reingest",
        DASHBOARD_BACKEND,
    ),
    ("GET", "/api/knowledge/files/{id}/text", DASHBOARD_BACKEND),
    (
        "GET",
        "/api/knowledge/files/{id}/original",
        DASHBOARD_BACKEND,
    ),
    ("POST", "/api/knowledge/search", DASHBOARD_BACKEND),
];

/// The reason string shared by every dashboard-internal mini-API row (owner
/// decision 2026-09-28): its shape follows the UI, not a published contract.
const DASHBOARD_BACKEND: &str =
    "The dashboard's own backend; its shapes follow the UI and are no contract.";
