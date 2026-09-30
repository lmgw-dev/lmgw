//! Docs-corpus mutations: `lmgw__docs_corpus_set`,
//! `lmgw__docs_ingest`, `lmgw__docs_request_set`.

use crate::mcp::selfadmin::{bool_p, enum_p, int_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__docs_corpus_set",
            writes: true,
            description:
                "Create or delete a documentation corpus — the owner's half of quickdoc, which \
                 the agent-facing `docs__*` tools deliberately cannot do. A corpus is one \
                 library at one version: `create` pins the two models it will use (both are \
                 resolved now, so a wrong alias fails here rather than inside the job), stores \
                 its sources, and unless you pass `start: false` queues the ingest immediately. \
                 Ingesting crawls pages and reads them with a model, so it is a background job \
                 — the reply carries its `job_id` and lmgw__docs_corpora reports how it is \
                 going. `delete` drops the corpus with its documents, chunks and vectors; there \
                 is no undo, and re-ingesting is a fresh crawl.",
            props: vec![
                ("action", enum_p("What to do.", &["create", "delete"])),
                (
                    "library",
                    str_p(
                        "create: library name, e.g. 'axum'. No '@' in it — the corpus id is \
                         `library@version`.",
                    ),
                ),
                (
                    "version",
                    str_p(
                        "create: the version these docs describe, e.g. '0.8'. Corpora are \
                         version-pinned; another version is another corpus, not an update.",
                    ),
                ),
                (
                    "embed_model",
                    str_p(
                        "create: alias to embed chunks with — an embedding model, usually \
                         `embed/<id>` (lmgw__models kind=aux lists them). The corpus pins the \
                         resolved identity, because a query has to be embedded by the same \
                         model; changing it later means a re-embed.",
                    ),
                ),
                (
                    "ingest_model",
                    str_p(
                        "create: chat alias that drives extraction — it reads each fetched page \
                         and emits the sections. Any capable local or cloud alias \
                         (lmgw__models); a long context helps more than raw size.",
                    ),
                ),
                (
                    "source_root",
                    str_p(
                        "create: where the documentation lives — one URL per line (an llms.txt, \
                         a docs index, a rustdoc JSON). Every line shares source_kind and fence.",
                    ),
                ),
                (
                    "source_kind",
                    enum_p(
                        "create: how to read those roots. Defaults to llms_txt. A starting hint \
                         only — every fetched document is sniffed and handled as whatever it \
                         actually turns out to be.",
                        &["llms_txt", "markdown", "rustdoc_json", "html"],
                    ),
                ),
                (
                    "fence",
                    str_p(
                        "create: domains the crawl may touch, one per line. Empty (the default) \
                         fences each source to its own host — the safe reading, not 'anywhere'.",
                    ),
                ),
                (
                    "start",
                    bool_p(
                        "create: queue the ingest as soon as the corpus exists. Default true; \
                         false leaves an empty corpus to start later with lmgw__docs_ingest.",
                    ),
                ),
                (
                    "corpus",
                    str_p("delete: numeric corpus id or `library@version`."),
                ),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__docs_ingest",
            writes: true,
            description: "Run or stop the job behind a corpus. `start` (re)crawls its sources and \
                 rebuilds it — the way to refresh docs that have moved on, and the way to run a \
                 corpus created with `start: false`; it is idempotent per corpus, so starting \
                 one that is already running returns that job rather than a second crawl. \
                 `re_embed` keeps the text and recomputes vectors only: what a corpus flagged \
                 're-embed required' needs, and what moving it to another embedding model \
                 takes. `cancel` asks a running job to stop, and it stops on its own terms. All \
                 three return as soon as the job is queued — lmgw__docs_corpora is the poll.",
            props: vec![
                (
                    "corpus",
                    str_p(
                        "Numeric corpus id or `library@version` — lmgw__docs_corpora lists both.",
                    ),
                ),
                (
                    "action",
                    enum_p("What to do.", &["start", "re_embed", "cancel"]),
                ),
                (
                    "embed_model",
                    str_p(
                        "re_embed only: move the corpus onto this embedding alias. Omitted, it \
                         fills in missing vectors with the model the corpus already pins.",
                    ),
                ),
            ],
            required: &["corpus", "action"],
        },
        Builtin {
            name: "lmgw__docs_request_set",
            writes: true,
            description:
                "Answer a doc request by hand: `dismissed` takes it off the queue (a library you \
                 will not ingest), `pending` puts it back. `fulfilled` is not settable — a \
                 request earns that when an ingest for its library@version finishes, so the way \
                 to fulfil one is lmgw__docs_corpus_set action=create.",
            props: vec![
                ("id", int_p("Request id, from lmgw__docs_requests.")),
                (
                    "status",
                    enum_p("Where to put it.", &["dismissed", "pending"]),
                ),
            ],
            required: &["id", "status"],
        },
    ]
}
