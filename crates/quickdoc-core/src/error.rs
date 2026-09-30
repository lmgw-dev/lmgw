//! One error type for the crate. quickdoc-core deliberately has no lmgw
//! dependency (§3), so it carries its own error rather than `GatewayError`;
//! the lmgw-core adapter maps it at the boundary.

#[derive(Debug, thiserror::Error)]
pub enum QuickdocError {
    #[error("corpus db error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("corpus migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("corpus io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no such corpus: {0}")]
    CorpusNotFound(String),
    /// §4's query-time verification. Never limp along: an alias remapped to a
    /// different model of the same width would otherwise poison every answer
    /// with no symptom, so the error names the corpus and both models.
    #[error(
        "corpus {corpus} is embedded with {pinned}, but the embedder resolves to {got} — \
         re-embed the corpus or query it with its pinned model"
    )]
    EmbedMismatch {
        corpus: String,
        pinned: String,
        got: String,
    },
    #[error("embedding for corpus {corpus} is {got} floats wide, expected {expected}")]
    DimsMismatch {
        corpus: String,
        expected: usize,
        got: usize,
    },
    #[error("embedder returned {got} vectors for {want} inputs")]
    EmbedCount { want: usize, got: usize },
    #[error("reranker returned {got} scores for {want} documents")]
    RerankCount { want: usize, got: usize },
    #[error("embedder failed: {0}")]
    Embedder(String),
    #[error("reranker failed: {0}")]
    Reranker(String),
    #[error("malformed corpus id {0:?} — expected `library@version`")]
    BadCorpusId(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, QuickdocError>;
