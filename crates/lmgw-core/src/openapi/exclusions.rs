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
//!
//! *Changed, 2026-10-10 (the owner's decision):* every lmgw HTTP API except
//! the dashboard itself goes into the description, typed — the Chat API, the
//! Knowledge bases API, the Audio lab and Image lab APIs, and the Chat
//! page's row refresh too. Their rows left this list as they were
//! documented. The thread, message, folder-patch and search rows of the
//! Chat API were the first to go (`planes/chat_threads.rs`), then the streaming
//! turns — send, continue, regenerate, a stored reply's read-aloud and the voice
//! warm-up (`planes/chat_turns.rs`), and then the attachments and exports
//! (`planes/chat_attachments.rs`). The Knowledge bases API followed
//! (`planes/knowledge.rs`, admin document only), and last the Audio lab
//! (`planes/audio_lab.rs`), the Image lab (`planes/image_lab.rs`) and the Chat
//! page's row refresh, `GET /chat/api/threads/rows`, which sits beside the
//! thread list (`planes/chat_threads.rs`). All are `Cap::Admin` routes and so
//! in the admin document only. The labs' four hand-offs to `/v1` handlers
//! (speech, transcriptions, alignments, tasks) reuse their siblings' schemas
//! and say how they differ; `DASHBOARD_BACKEND`, the shared reason of the
//! 2026-09-28 decision, went with its last user.
//!
//! **What stays, as of 2026-10-10:** the dashboard SPA and its bundle
//! (`/`, `/{*path}`), the legacy `/ui` redirects, and the agent app and agent
//! MCP reverse proxies (`/agents/{id}/app*`, `/agents/{id}/mcp*`: the agent's
//! own surface). Nothing else in `CAPABILITY_TABLE` is undocumented.

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
];

/// `(method, path, reason)` — every operation whose answer is
/// `Resp::Untyped`, by the document's own spelling (`get`/`post`, path with
/// `{id}`). Only a verbatim relay of another program's JSON belongs here: the
/// bytes are that program's, so lmgw has no type to build them from. Anything
/// else gives its answer an `lmgw-api-types` type the handler builds.
/// `build.rs`'s `untyped_responses_are_exactly_the_allowlist` fails both ways:
/// a new untyped answer or request body not listed, a listed one that no longer is.
#[cfg(test)]
pub(crate) const UNTYPED: &[(&str, &str, &str)] = &[
    (
        "post",
        "/v1/audio/alignments",
        "audio.cpp's own alignment JSON, relayed verbatim.",
    ),
    (
        "post",
        "/audio-lab/api/alignments",
        "audio.cpp's own alignment JSON, relayed verbatim.",
    ),
    (
        "post",
        "/v1/tasks/run",
        "audio.cpp's own per-task JSON, relayed verbatim.",
    ),
    (
        "post",
        "/v1/tasks/stream",
        "audio.cpp's own per-task stream, relayed verbatim.",
    ),
    (
        "post",
        "/audio-lab/api/tasks/run",
        "audio.cpp's own per-task JSON, relayed verbatim.",
    ),
];
