//! Device pairing (device link design §3.3, §3.6): the one pairing token a
//! device row holds, the enrolment that uses it, the owner's confirmation,
//! and the reset the CA's rotation makes. Every write in one transaction
//! that holds the write lock from its first statement (`begin_write`), so
//! two devices racing with one token serialize on it.
//!
//! Times are RFC 3339 UTC to the second with `Z` (`stamp`), so the columns
//! compare as text in the order they compare as times.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqliteConnection, SqlitePool};

use super::*;
use crate::config::{DeviceLink, KeyPolicy};

/// A time as the pairing columns hold it.
pub fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A pairing token to write: its SHA-256 (the plaintext is the owner's
/// once, in the link) and its life.
#[derive(Debug, Clone)]
pub struct NewToken {
    pub hash: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// A device row's pairing token, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRow {
    pub key_id: i64,
    pub created_at: String,
    pub expires_at: String,
    pub used_at: Option<String>,
    pub used_from: Option<String>,
    /// The enrolling key's `spki:` hash.
    pub used_by: Option<String>,
    pub ended_at: Option<String>,
}

impl TokenRow {
    /// Alive at `now`: not lapsed and not ended. A used token stays alive
    /// for the same key's repeat (§3.3's note of 2026-10-15).
    pub fn alive(&self, now: DateTime<Utc>) -> bool {
        self.ended_at.is_none() && self.expires_at > stamp(now)
    }
}

fn token_from_row(r: &sqlx::sqlite::SqliteRow) -> TokenRow {
    TokenRow {
        key_id: r.get("key_id"),
        created_at: r.get("created_at"),
        expires_at: r.get("expires_at"),
        used_at: r.get("used_at"),
        used_from: r.get("used_from"),
        used_by: r.get("used_by"),
        ended_at: r.get("ended_at"),
    }
}

async fn write_token(tx: &mut SqliteConnection, key_id: i64, token: &NewToken) -> DbResult<()> {
    sqlx::query(
        "INSERT INTO device_pairing_tokens (key_id, token_hash, created_at, expires_at)
              VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (key_id) DO UPDATE
               SET token_hash = excluded.token_hash, created_at = excluded.created_at,
                   expires_at = excluded.expires_at, used_at = NULL, used_from = NULL,
                   used_by = NULL, certificate = NULL, ended_at = NULL",
    )
    .bind(key_id)
    .bind(&token.hash)
    .bind(stamp(token.created_at))
    .bind(stamp(token.expires_at))
    .execute(tx)
    .await?;
    Ok(())
}

/// A device row born with its policy, its URLs and its first pairing token
/// (§3.2), in one transaction: `key_hash` is `pending:…` until the device
/// enrols. The hosting label and the admin level as
/// [`insert_device_key`] writes them.
#[allow(clippy::too_many_arguments)]
pub async fn insert_paired_device(
    pool: &SqlitePool,
    name: &str,
    policy: &KeyPolicy,
    grant: (Option<&str>, crate::config::DeviceAdmin),
    note: &str,
    (urls, local_only): (&[String], bool),
    token: &NewToken,
) -> DbResult<i64> {
    let mut tx = super::begin_write(pool).await?;
    let id = super::keys::insert_device_row(
        &mut tx,
        name,
        &DeviceLink::pending_hash(),
        policy,
        grant,
        note,
    )
    .await?;
    sqlx::query("UPDATE api_keys SET urls = ?2, local_only = ?3 WHERE id = ?1")
        .bind(id)
        .bind(serde_json::to_string(urls).unwrap_or_else(|_| "[]".into()))
        .bind(i64::from(local_only))
        .execute(&mut *tx)
        .await?;
    write_token(&mut tx, id, token).await?;
    tx.commit().await?;
    Ok(id)
}

/// Device `key_id`'s URL list (`key_set`'s `urls`, §3.1).
pub async fn set_device_urls(pool: &SqlitePool, key_id: i64, urls: &[String]) -> DbResult<()> {
    sqlx::query("UPDATE api_keys SET urls = ?2 WHERE id = ?1 AND kind = 'device'")
        .bind(key_id)
        .bind(serde_json::to_string(urls).unwrap_or_else(|_| "[]".into()))
        .execute(pool)
        .await?;
    Ok(())
}

/// What [`new_device_token`] found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenIssued {
    /// The row's credential before: an enrolled key's `spki:` hash, when
    /// `end_current` ended it.
    pub ended_key: Option<String>,
}

/// A new pairing token for device `key_id` (§5.2), replacing its earlier
/// one. With `end_current` the current credential ends at once: `key_hash`
/// becomes `pending:…` and the certificate and confirmation columns are
/// cleared, as a stolen key needs. Either way a key that waited for its
/// confirmation beside the row's is dropped with the token that enrolled
/// it, and so is the contested mark of that token. `link` replaces the
/// row's URLs and its "this computer" flag when given. `None` when no
/// device row has the id.
pub async fn new_device_token(
    pool: &SqlitePool,
    key_id: i64,
    token: &NewToken,
    end_current: bool,
    link: Option<(&[String], bool)>,
) -> DbResult<Option<TokenIssued>> {
    let mut tx = super::begin_write(pool).await?;
    let current: Option<String> =
        sqlx::query_scalar("SELECT key_hash FROM api_keys WHERE id = ?1 AND kind = 'device'")
            .bind(key_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(current) = current else {
        return Ok(None);
    };
    let mut ended_key = None;
    if end_current && !current.starts_with(DeviceLink::PENDING_PREFIX) {
        clear_key(&mut tx, key_id).await?;
        // A key from before TLS opened nothing to end.
        ended_key = current
            .starts_with(DeviceLink::KEY_PREFIX)
            .then_some(current);
    }
    sqlx::query(
        "UPDATE api_keys
            SET waiting_key_hash = NULL, waiting_cert_serial = NULL,
                waiting_cert_not_after = NULL, waiting_cert_notice_after = NULL,
                contested_at = NULL, contested_from = NULL
          WHERE id = ?1",
    )
    .bind(key_id)
    .execute(&mut *tx)
    .await?;
    if let Some((urls, local_only)) = link {
        sqlx::query("UPDATE api_keys SET urls = ?2, local_only = ?3 WHERE id = ?1")
            .bind(key_id)
            .bind(serde_json::to_string(urls).unwrap_or_else(|_| "[]".into()))
            .bind(i64::from(local_only))
            .execute(&mut *tx)
            .await?;
    }
    write_token(&mut tx, key_id, token).await?;
    tx.commit().await?;
    Ok(Some(TokenIssued { ended_key }))
}

/// A device row's key ends: `pending:…`, its certificate and confirmation
/// cleared, and a key waiting beside it with them; its policy, URLs and
/// history kept.
async fn clear_key(tx: &mut SqliteConnection, key_id: i64) -> DbResult<()> {
    sqlx::query(
        "UPDATE api_keys
            SET key_hash = ?2, cert_serial = NULL, cert_not_after = NULL,
                cert_notice_after = NULL, confirmed_at = NULL, waiting_key_hash = NULL,
                waiting_cert_serial = NULL, waiting_cert_not_after = NULL,
                waiting_cert_notice_after = NULL
          WHERE id = ?1 AND kind = 'device'",
    )
    .bind(key_id)
    .bind(DeviceLink::pending_hash())
    .execute(tx)
    .await?;
    Ok(())
}

/// End device `key_id`'s token if it is alive at `now` (the QR card's
/// close, §3.3): whether one was.
pub async fn end_device_token(
    pool: &SqlitePool,
    key_id: i64,
    now: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "UPDATE device_pairing_tokens SET ended_at = ?2
          WHERE key_id = ?1 AND ended_at IS NULL AND expires_at > ?2",
    )
    .bind(key_id)
    .bind(stamp(now))
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Every device row's token, alive or not: the Keys list's pairing view.
pub async fn device_tokens(pool: &SqlitePool) -> DbResult<Vec<TokenRow>> {
    let rows = sqlx::query("SELECT * FROM device_pairing_tokens ORDER BY key_id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(token_from_row).collect())
}

/// A token alive, as the handshake's "a token is pending" starts from it
/// (`devices::PendingTokens`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliveToken {
    pub key_id: i64,
    pub expires_at: DateTime<Utc>,
    /// The certificate its enrolment answered (standard base64 DER), once
    /// it was used.
    pub answered: Option<String>,
}

/// The tokens alive at `now`.
pub async fn alive_tokens(pool: &SqlitePool, now: DateTime<Utc>) -> DbResult<Vec<AliveToken>> {
    let rows = sqlx::query(
        "SELECT key_id, expires_at, certificate FROM device_pairing_tokens
          WHERE ended_at IS NULL AND expires_at > ?1",
    )
    .bind(stamp(now))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let at: String = r.get("expires_at");
            DateTime::parse_from_rfc3339(&at).ok().map(|t| AliveToken {
                key_id: r.get("key_id"),
                expires_at: t.with_timezone(&Utc),
                answered: r.get("certificate"),
            })
        })
        .collect())
}

/// End device `key_id`'s token at `now` if it is alive and used: its device
/// made a request with the certificate the token's enrolment answered, so
/// it holds its answer and needs no repeat (D8, §3.3's note of 2026-10-15).
/// Whether one was. A token minted since, unused, stays.
pub async fn end_answered_token(
    pool: &SqlitePool,
    key_id: i64,
    now: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "UPDATE device_pairing_tokens SET ended_at = ?2
          WHERE key_id = ?1 AND used_at IS NOT NULL AND ended_at IS NULL AND expires_at > ?2",
    )
    .bind(key_id)
    .bind(stamp(now))
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// An enrolment's request (§3.4), checked and hashed.
#[derive(Debug, Clone)]
pub struct EnrolAsk<'a> {
    /// The SHA-256 of the presented token.
    pub token_hash: &'a str,
    /// The enrolling key's `spki:` hash.
    pub key_hash: &'a str,
    /// The peer address.
    pub from: &'a str,
    pub client: Option<&'a str>,
    pub now: DateTime<Utc>,
}

/// A device certificate issued inside the enrolment's transaction, as the
/// row keeps it.
#[derive(Debug, Clone)]
pub struct Issued {
    pub der_b64: String,
    pub serial: String,
    pub not_after: DateTime<Utc>,
    pub notice_after: DateTime<Utc>,
}

/// The device row an enrolment answers for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolledRow {
    pub key_id: i64,
    /// `device:<name>`.
    pub name: String,
    pub urls: Vec<String>,
    pub certificate_b64: String,
    pub not_after: String,
    pub notice_after: String,
    pub confirmed: bool,
}

/// How an enrolment's key joined its row (§3.4, §3.5, §5.2, §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Joined {
    /// The row's key now, unconfirmed: the row held none (`pending:…`), one
    /// from before TLS, or an enrolled key never confirmed, which `previous`
    /// names.
    Replaced { previous: String },
    /// The key the row holds already, with a fresh certificate: a renewal,
    /// its confirmation kept.
    Renewed,
    /// A new key beside the row's confirmed one, which keeps working until
    /// the owner confirms this one (a re-pair that kept the current key).
    Waiting,
}

/// What an enrolment came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enrolled<E> {
    /// The token was used now: the answer for `row`, the key joined as
    /// `joined` says, its certificate's serial `serial`.
    New {
        row: EnrolledRow,
        joined: Joined,
        serial: String,
    },
    /// The same key presented the token it used, within the token's life:
    /// the first answer again, nothing changed (§3.3).
    Repeat(EnrolledRow),
    /// No token has that hash.
    Unknown,
    /// The token lapsed at `at`.
    Expired { at: String },
    /// The owner ended it at `at`.
    Ended { at: String },
    /// Another key used it: the row is marked contested, `newly` when this
    /// is the first such presentation of the row's latest token.
    UsedElsewhere {
        key_id: i64,
        name: String,
        newly: bool,
    },
    /// The token's device is disabled; the token stays unused.
    Disabled { name: String },
    /// Another device row holds the key, or waits with it.
    KeyInUse { name: String },
    /// The certificate could not be issued; nothing changed.
    NotIssued(E),
}

/// The enrolment's write (§3.3, §3.4), in one transaction: take the token
/// (single use), else tell the cases apart; read its device; refuse a key
/// another device holds or waits with; `issue` the certificate for the
/// row's name (`device:` cut); write the key as [`Joined`] says, with the
/// certificate's columns and the enrolment's, and the answer's certificate
/// on the token, so its repeat is the same answer.
///
/// *Changed 2026-10-15 (§3.5, §5.3):* a re-enrolment never ends a working
/// key here. The key the row holds is renewed, its confirmation kept; a new
/// key beside a confirmed one waits for the owner's Confirm
/// ([`confirm_device`]), which ends the kept one. Only a row with no
/// working key takes the new one as its key, unconfirmed.
pub async fn enrol_device<E>(
    pool: &SqlitePool,
    ask: &EnrolAsk<'_>,
    issue: impl FnOnce(&str) -> Result<Issued, E>,
) -> DbResult<Enrolled<E>> {
    let now = stamp(ask.now);
    let mut tx = super::begin_write(pool).await?;
    let taken: Option<i64> = sqlx::query_scalar(
        "UPDATE device_pairing_tokens SET used_at = ?2, used_from = ?3, used_by = ?4
          WHERE token_hash = ?1 AND used_at IS NULL AND ended_at IS NULL AND expires_at > ?2
         RETURNING key_id",
    )
    .bind(ask.token_hash)
    .bind(&now)
    .bind(ask.from)
    .bind(ask.key_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(key_id) = taken else {
        return not_taken(tx, ask, &now).await;
    };
    let row = sqlx::query(
        "SELECT name, enabled, key_hash, confirmed_at FROM api_keys
          WHERE id = ?1 AND kind = 'device'",
    )
    .bind(key_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        // A token whose row went without it: none, as far as anyone asking
        // can tell (the delete takes both in one transaction).
        return Ok(Enrolled::Unknown);
    };
    let name: String = row.get("name");
    if row.get::<i64, _>("enabled") == 0 {
        // Dropped: the token stays unused.
        return Ok(Enrolled::Disabled { name });
    }
    let previous: String = row.get("key_hash");
    let confirmed = row.get::<Option<String>, _>("confirmed_at").is_some();
    let holder: Option<String> = sqlx::query_scalar(
        "SELECT name FROM api_keys
          WHERE kind = 'device' AND (key_hash = ?1 OR waiting_key_hash = ?1) AND id != ?2",
    )
    .bind(ask.key_hash)
    .bind(key_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(holder) = holder {
        return Ok(Enrolled::KeyInUse { name: holder });
    }
    let issued = match issue(crate::devices::short_name(&name)) {
        Ok(i) => i,
        Err(e) => return Ok(Enrolled::NotIssued(e)),
    };
    let joined = if previous == ask.key_hash {
        Joined::Renewed
    } else if confirmed && previous.starts_with(DeviceLink::KEY_PREFIX) {
        Joined::Waiting
    } else {
        Joined::Replaced { previous }
    };
    let write = match joined {
        // The certificate is new; the key, and the owner's word on it, are
        // not.
        Joined::Renewed => {
            "UPDATE api_keys
                SET cert_serial = ?3, cert_not_after = ?4, cert_notice_after = ?5,
                    enrolled_at = ?6, enrolled_from = ?7, client_info = ?8
              WHERE id = ?1 AND key_hash = ?2"
        }
        Joined::Waiting => {
            "UPDATE api_keys
                SET waiting_key_hash = ?2, waiting_cert_serial = ?3,
                    waiting_cert_not_after = ?4, waiting_cert_notice_after = ?5,
                    enrolled_at = ?6, enrolled_from = ?7, client_info = ?8
              WHERE id = ?1"
        }
        Joined::Replaced { .. } => {
            "UPDATE api_keys
                SET key_hash = ?2, cert_serial = ?3, cert_not_after = ?4, cert_notice_after = ?5,
                    enrolled_at = ?6, enrolled_from = ?7, client_info = ?8, confirmed_at = NULL,
                    waiting_key_hash = NULL, waiting_cert_serial = NULL,
                    waiting_cert_not_after = NULL, waiting_cert_notice_after = NULL
              WHERE id = ?1"
        }
    };
    sqlx::query(write)
        .bind(key_id)
        .bind(ask.key_hash)
        .bind(&issued.serial)
        .bind(stamp(issued.not_after))
        .bind(stamp(issued.notice_after))
        .bind(&now)
        .bind(ask.from)
        .bind(ask.client)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE device_pairing_tokens SET certificate = ?2 WHERE key_id = ?1")
        .bind(key_id)
        .bind(&issued.der_b64)
        .execute(&mut *tx)
        .await?;
    let urls = row_urls(&mut tx, key_id).await?;
    tx.commit().await?;
    Ok(Enrolled::New {
        row: EnrolledRow {
            key_id,
            name,
            urls,
            certificate_b64: issued.der_b64,
            not_after: stamp(issued.not_after),
            notice_after: stamp(issued.notice_after),
            confirmed: joined == Joined::Renewed && confirmed,
        },
        joined,
        serial: issued.serial,
    })
}

async fn row_urls(tx: &mut SqliteConnection, key_id: i64) -> DbResult<Vec<String>> {
    let urls: Option<String> = sqlx::query_scalar("SELECT urls FROM api_keys WHERE id = ?1")
        .bind(key_id)
        .fetch_optional(tx)
        .await?;
    Ok(urls
        .and_then(|u| serde_json::from_str(&u).ok())
        .unwrap_or_default())
}

/// The token was not taken: which case it is (§3.3). Only the contested
/// mark writes, and commits, the first time; every other case changes
/// nothing.
async fn not_taken<E>(
    mut tx: sqlx::Transaction<'static, sqlx::Sqlite>,
    ask: &EnrolAsk<'_>,
    now: &str,
) -> DbResult<Enrolled<E>> {
    let token = sqlx::query(
        "SELECT t.*, k.name, k.enabled, k.key_hash, k.cert_not_after, k.cert_notice_after,
                k.confirmed_at, k.contested_at, k.waiting_key_hash, k.waiting_cert_not_after,
                k.waiting_cert_notice_after
           FROM device_pairing_tokens t JOIN api_keys k ON k.id = t.key_id
          WHERE t.token_hash = ?1 AND k.kind = 'device'",
    )
    .bind(ask.token_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(r) = token else {
        return Ok(Enrolled::Unknown);
    };
    let t = token_from_row(&r);
    let name: String = r.get("name");
    match t.used_by.as_deref() {
        Some(by) if by == ask.key_hash => {
            if let Some(at) = t.ended_at {
                return Ok(Enrolled::Ended { at });
            }
            if t.expires_at.as_str() <= now {
                return Ok(Enrolled::Expired { at: t.expires_at });
            }
            if r.get::<i64, _>("enabled") == 0 {
                return Ok(Enrolled::Disabled { name });
            }
            // Still the row's key, or the key waiting beside it, with the
            // certificate the first answer carried: the same answer. A row
            // whose key moved on since (it cannot without a new token,
            // which replaces this one) answers as a token that is gone.
            let certificate: Option<String> = r.get("certificate");
            let holds: String = r.get("key_hash");
            let waits: Option<String> = r.get("waiting_key_hash");
            let (dates, confirmed) = if holds == ask.key_hash {
                (
                    ("cert_not_after", "cert_notice_after"),
                    r.get::<Option<String>, _>("confirmed_at").is_some(),
                )
            } else if waits.as_deref() == Some(ask.key_hash) {
                (
                    ("waiting_cert_not_after", "waiting_cert_notice_after"),
                    false,
                )
            } else {
                return Ok(Enrolled::Unknown);
            };
            let Some(certificate) = certificate else {
                return Ok(Enrolled::Unknown);
            };
            let urls = row_urls(&mut tx, t.key_id).await?;
            Ok(Enrolled::Repeat(EnrolledRow {
                key_id: t.key_id,
                name,
                urls,
                certificate_b64: certificate,
                not_after: r.get::<Option<String>, _>(dates.0).unwrap_or_default(),
                notice_after: r.get::<Option<String>, _>(dates.1).unwrap_or_default(),
                confirmed,
            }))
        }
        Some(_) => {
            // Marked once per token: a new token clears the mark, so a
            // second presentation of this one says nothing new.
            let newly = r.get::<Option<String>, _>("contested_at").is_none();
            if newly {
                sqlx::query(
                    "UPDATE api_keys SET contested_at = ?2, contested_from = ?3
                      WHERE id = ?1 AND kind = 'device'",
                )
                .bind(t.key_id)
                .bind(now)
                .bind(ask.from)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
            }
            Ok(Enrolled::UsedElsewhere {
                key_id: t.key_id,
                name,
                newly,
            })
        }
        None => match t.ended_at {
            Some(at) => Ok(Enrolled::Ended { at }),
            None => Ok(Enrolled::Expired { at: t.expires_at }),
        },
    }
}

/// What [`confirm_device`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmed {
    /// Confirmed now; the row's key's `spki:` hash. `replaced` is the key
    /// the row kept until now, when the confirmed key waited beside it: it
    /// ends with this write.
    Now {
        key_hash: String,
        replaced: Option<String>,
    },
    /// It was confirmed already, at `at`, and no key waits.
    Already { key_hash: String, at: String },
    /// The code is not the one of the key waiting for the confirmation (or,
    /// with none waiting, of the confirmed key): nothing changed.
    Mismatch,
    /// The row holds no enrolled key: nothing to confirm.
    NotEnrolled,
    /// No device row has the id.
    Missing,
}

/// The owner's confirmation of device `key_id`'s enrolled key (§3.5), the
/// one whose code is `code` (`7F3A-91C2`): the key that waits beside a
/// kept one, else the row's own. A waiting key becomes the row's key, with
/// its certificate's columns, and the kept one ends; the code is checked in
/// the same transaction, so a key enrolled after the owner read the code is
/// never confirmed in its place.
pub async fn confirm_device(
    pool: &SqlitePool,
    key_id: i64,
    code: &str,
    now: DateTime<Utc>,
) -> DbResult<Confirmed> {
    let mut tx = super::begin_write(pool).await?;
    let row = sqlx::query(
        "SELECT key_hash, confirmed_at, waiting_key_hash FROM api_keys
          WHERE id = ?1 AND kind = 'device'",
    )
    .bind(key_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(Confirmed::Missing);
    };
    let key_hash: String = row.get("key_hash");
    let coded = |hash: &str| DeviceLink::confirm_code(hash).as_deref() == Some(code);
    if let Some(waiting) = row.get::<Option<String>, _>("waiting_key_hash") {
        if !coded(&waiting) {
            return Ok(Confirmed::Mismatch);
        }
        sqlx::query(
            "UPDATE api_keys
                SET key_hash = waiting_key_hash, cert_serial = waiting_cert_serial,
                    cert_not_after = waiting_cert_not_after,
                    cert_notice_after = waiting_cert_notice_after, confirmed_at = ?2,
                    waiting_key_hash = NULL, waiting_cert_serial = NULL,
                    waiting_cert_not_after = NULL, waiting_cert_notice_after = NULL
              WHERE id = ?1",
        )
        .bind(key_id)
        .bind(stamp(now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(Confirmed::Now {
            key_hash: waiting,
            replaced: Some(key_hash),
        });
    }
    if !key_hash.starts_with(DeviceLink::KEY_PREFIX) {
        return Ok(Confirmed::NotEnrolled);
    }
    if !coded(&key_hash) {
        return Ok(Confirmed::Mismatch);
    }
    if let Some(at) = row.get::<Option<String>, _>("confirmed_at") {
        return Ok(Confirmed::Already { key_hash, at });
    }
    sqlx::query("UPDATE api_keys SET confirmed_at = ?2 WHERE id = ?1")
        .bind(key_id)
        .bind(stamp(now))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Confirmed::Now {
        key_hash,
        replaced: None,
    })
}

/// The CA's rotation (§5.5), in one transaction: every enrolled device row
/// loses its key (`pending:…`, certificate and confirmation cleared, policy
/// kept), and every token alive ends, since its link pins the old CA. The
/// rows reset, `(id, name)`.
pub async fn reset_devices_for_new_ca(
    pool: &SqlitePool,
    now: DateTime<Utc>,
) -> DbResult<Vec<(i64, String)>> {
    let mut tx = super::begin_write(pool).await?;
    let rows = sqlx::query(
        "SELECT id, name FROM api_keys WHERE kind = 'device' AND key_hash LIKE 'spki:%' ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let reset: Vec<(i64, String)> = rows.iter().map(|r| (r.get("id"), r.get("name"))).collect();
    for (id, _) in &reset {
        clear_key(&mut tx, *id).await?;
    }
    sqlx::query(
        "UPDATE device_pairing_tokens SET ended_at = ?1 WHERE ended_at IS NULL AND expires_at > ?1",
    )
    .bind(stamp(now))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(reset)
}
