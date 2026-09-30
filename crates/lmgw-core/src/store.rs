//! SQLite persistence via sqlx (§9). All config reads on the hot path go
//! through the in-memory [`Snapshot`](crate::config::Snapshot); this module is the cold path.

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

use crate::error::GatewayError;

pub type DbResult<T> = Result<T, GatewayError>;

/// Open (creating if needed) the SQLite DB, run migrations, enforce 0600.
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    // Secrets are plaintext in this file (§13): clamp permissions. **Before**
    // the migrator as well as after it — a migration rewrites whole tables
    // (0032 rebuilds `api_keys`, key material and all) and the WAL it writes
    // through holds those pages in the clear, so a first-boot window in which
    // the sidecars are world-readable is a window in which the secrets are.
    clamp_0600(path)?;
    run_migrations(&pool).await?;
    clamp_0600(path)?;
    Ok(pool)
}

/// `0600` on the database **and both WAL sidecars**.
///
/// `-wal` and `-shm` are not incidental: in WAL mode a committed page lives in
/// `lmgw.sqlite-wal` until the next checkpoint, so every secret this file holds
/// is readable there too, and SQLite creates them with the process umask rather
/// than inheriting the main file's mode. Clamping one path and calling it done
/// was the hole.
///
/// A sidecar that is not there yet is skipped rather than created: they appear
/// with the first write, and the second call (after the migrator) is what
/// catches them.
#[cfg(unix)]
pub(crate) fn clamp_0600(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let stem = path.as_os_str().to_owned();
    for suffix in ["", "-wal", "-shm"] {
        let mut name = stem.clone();
        name.push(suffix);
        let p = std::path::PathBuf::from(name);
        if p.exists() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn clamp_0600(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// In-memory DB for tests.
pub async fn open_in_memory() -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    run_migrations(&pool).await?;
    Ok(pool)
}

/// Bring an open pool's schema up to date: repairs first, then the migrator.
///
/// Public because it is also the seam a test upgrades a hand-built old database
/// through — there is no other honest way to prove that an install stuck at an
/// earlier version can still start.
pub async fn run_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
    repair_before_migrations(pool).await?;
    refuse_if_aliases_pin_the_managed_upstreams(pool).await?;
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

mod migration_guards;
use migration_guards::*;

mod rows;
use rows::*;

mod snapshot;
pub use snapshot::*;

mod upstreams;
pub use upstreams::*;

mod aliases;
pub use aliases::*;

mod candidate_aliases;
pub use candidate_aliases::*;

mod local_models;
pub use local_models::*;

mod aux_models;
pub use aux_models::*;

mod audio_models;
pub use audio_models::*;

mod image_models;
pub use image_models::*;

mod downloads;
pub use downloads::*;

mod jobs_table;
pub use jobs_table::*;

mod builds;
pub use builds::*;

mod bench;
pub use bench::*;

mod keys;
pub use keys::*;

mod mcp_servers;
pub use mcp_servers::*;

mod request_logs;
pub use request_logs::*;

mod agent_catalog;
pub use agent_catalog::*;

mod chat;
pub use chat::*;

mod chat_attachments;
pub use chat_attachments::*;

mod chat_attachment_extra;
pub use chat_attachment_extra::*;

mod chat_folders;
pub use chat_folders::*;

mod chat_keep;
pub use chat_keep::*;

mod chat_knowledge;
pub use chat_knowledge::*;

mod chat_search;
pub use chat_search::*;

mod chat_messages;
pub use chat_messages::*;

mod responses;
pub use responses::*;

mod prices;
pub use prices::*;

mod usage;
pub use usage::*;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod usage_tests;
