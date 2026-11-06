//! The `fallback` column every model and alias table has (openai egress
//! design §14.2, migration 0095), as one write: the references a rename
//! rewrites in its own transaction (decision 27).

use super::*;

/// The six tables whose rows carry a fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FallbackTable {
    LocalModels,
    AuxModels,
    AudioModels,
    ImageModels,
    CandidateAliases,
    /// Plain aliases.
    Models,
}

impl FallbackTable {
    fn table(self) -> &'static str {
        match self {
            Self::LocalModels => "local_models",
            Self::AuxModels => "aux_models",
            Self::AudioModels => "audio_models",
            Self::ImageModels => "image_models",
            Self::CandidateAliases => "candidate_aliases",
            Self::Models => "models",
        }
    }
}

/// One row's fallback, as a rename rewrites it: the new public name of what
/// it named, and the name it held when the rewrite was computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackRewrite {
    pub table: FallbackTable,
    pub id: i64,
    /// The stored value the rewrite was computed from.
    pub was: String,
    pub fallback: String,
}

/// Write `rewrites` inside the caller's transaction, the one its rename is
/// written in, so no snapshot ever sees the new name beside a fallback that
/// still names the old one.
///
/// Each row is rewritten only while it still holds the name the rewrite was
/// computed from (`was`): the rewrites are computed from the snapshot before
/// the rename's checks, which can read upstream catalogs for seconds, and a
/// fallback another write changed or cleared meanwhile is that write's, not
/// this one's to undo.
pub async fn rewrite_fallbacks_in(
    conn: &mut sqlx::SqliteConnection,
    rewrites: &[FallbackRewrite],
) -> DbResult<()> {
    for r in rewrites {
        let sql = format!(
            "UPDATE {} SET fallback = ?2, updated_at = datetime('now') \
             WHERE id = ?1 AND fallback = ?3",
            r.table.table()
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(r.id)
            .bind(&r.fallback)
            .bind(&r.was)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}
