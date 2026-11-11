//! The refusals a container start asks ahead of its migration copy
//! (headless server design D113): the guards that only read, so a start
//! they refuse makes no copy at all, and a unit that restarts it
//! (`Restart=always`) does not add one copy per refusal.

use sqlx::SqlitePool;

use super::{
    migration_applied, refuse_if_aliases_pin_the_managed_upstreams, refuse_unmappable_price_rows,
    refuse_unreadable_global, table_exists, AUX_MIGRATION,
};

/// Which refusals ran ahead of the copy, so the migration's own path does
/// not ask them again.
#[derive(Debug, Clone, Copy)]
pub(in crate::store) struct BeforeCopy {
    /// The 0023 alias guard and 0095's refusal ran too (0018 was recorded:
    /// its repair, which renames upstreams and rewrites the settings blob
    /// both read, has nothing left to do).
    pub(in crate::store) aliases: bool,
}

/// 0070's price rows always; 0023's aliases and 0095's global fallback
/// once 0018 is recorded. On a database from before 0018 the repair ahead
/// of the migrator renames what those two read, so they wait for it there,
/// after the copy, as on the desktop.
pub(in crate::store) async fn refuse_before_copy(pool: &SqlitePool) -> anyhow::Result<BeforeCopy> {
    refuse_unmappable_price_rows(pool).await?;
    let aliases = table_exists(pool, "_sqlx_migrations").await?
        && migration_applied(pool, AUX_MIGRATION).await?;
    if aliases {
        refuse_if_aliases_pin_the_managed_upstreams(pool).await?;
        refuse_unreadable_global(pool).await?;
    }
    Ok(BeforeCopy { aliases })
}
