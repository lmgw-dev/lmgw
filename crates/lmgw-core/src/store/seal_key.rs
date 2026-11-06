//! The install's seal key (`ir::seal`'s key module, WP8 verify V-N2): 32
//! random bytes, generated once and kept in the `settings` table under
//! [`SEAL_KEY`], hex, beside the install's other secrets in the 0600
//! database. Nothing but [`seal_key`] reads the row: no API, op, log line or
//! export.

use sqlx::SqlitePool;

use crate::ir::SEAL_KEY_LEN;

use super::DbResult;

/// The `settings` key the seal key is kept under.
pub const SEAL_KEY: &str = "seals:config_key";

/// This install's seal key: the one kept, or one generated and kept now, in
/// one write, so two starts on one database never keep two.
pub async fn seal_key(pool: &SqlitePool) -> DbResult<[u8; SEAL_KEY_LEN]> {
    let drawn: [u8; SEAL_KEY_LEN] = rand::random();
    let mut tx = super::begin_write(pool).await?;
    sqlx::query("INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO NOTHING")
        .bind(SEAL_KEY)
        .bind(hex::encode(drawn))
        .execute(&mut *tx)
        .await?;
    let kept: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?1")
        .bind(SEAL_KEY)
        .fetch_one(&mut *tx)
        .await?;
    let key = match hex::decode(&kept).ok().and_then(|b| b.try_into().ok()) {
        Some(key) => key,
        // Not one this process wrote (a hand edit): replaced, so the
        // fingerprints are keyed again. The seals made under it stop going
        // back, their text kept, as after a row's key change (openai egress
        // design §7.3.1).
        None => {
            tracing::warn!(
                "the seal key row does not read as {SEAL_KEY_LEN} bytes; a new one is kept"
            );
            sqlx::query("UPDATE settings SET value = ?2 WHERE key = ?1")
                .bind(SEAL_KEY)
                .bind(hex::encode(drawn))
                .execute(&mut *tx)
                .await?;
            drawn
        }
    };
    tx.commit().await?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_key_is_generated_once_and_kept() {
        let pool = super::super::open_in_memory().await.unwrap();
        let first = seal_key(&pool).await.unwrap();
        assert_eq!(seal_key(&pool).await.unwrap(), first);
        let other = super::super::open_in_memory().await.unwrap();
        assert_ne!(seal_key(&other).await.unwrap(), first, "per install");
        sqlx::query("UPDATE settings SET value = 'nope' WHERE key = ?1")
            .bind(SEAL_KEY)
            .execute(&pool)
            .await
            .unwrap();
        let replaced = seal_key(&pool).await.unwrap();
        assert_eq!(seal_key(&pool).await.unwrap(), replaced);
    }
}
