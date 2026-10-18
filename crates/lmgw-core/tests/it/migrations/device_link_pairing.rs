//! Migration 0081: the device link's pairing (device link design §3.6, §12).
//! Every device row is marked `v1:` (its bearer opens nothing any more, and
//! the refusal names it), keeps its id, policy and grant, and reads with no
//! URLs, no certificate and no confirmation; other kinds keep their hash. The
//! token table exists, one row per device; two device rows cannot hold one
//! key; request rows say which listener they came in on.

use lmgw_core::config::{ApiKeyKind, DeviceCredential, DeviceLink};

use super::db_at_version;

#[tokio::test]
async fn migration_0081_marks_every_device_paired_before_tls() {
    let pool = db_at_version(80).await;
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, kind, hosts_label, rpm_limit)
         VALUES (50, 'device:desk', 'h50', 'device', 'desk', 30),
                (51, 'laptop', 'h51', 'key', NULL, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO request_logs (ts, ingress_proto, requested_alias, status) VALUES ('2026-10-14 10:00:00', 'openai', 'm', 200)")
        .execute(&pool)
        .await
        .unwrap();

    lmgw_core::store::run_migrations(&pool).await.unwrap();

    let snap = lmgw_core::store::load_snapshot(&pool).await.unwrap();
    let desk = snap.api_keys.iter().find(|k| k.id == 50).unwrap();
    assert_eq!(desk.kind, ApiKeyKind::Device);
    assert_eq!(desk.key_hash, "v1:h50");
    assert_eq!(DeviceLink::credential(&desk.key_hash), DeviceCredential::V1);
    assert_eq!(
        (desk.policy.rpm_limit, desk.hosts_label.as_deref()),
        (30, Some("desk"))
    );
    assert_eq!(
        desk.link,
        DeviceLink::default(),
        "nothing paired over TLS yet"
    );
    let laptop = snap.api_keys.iter().find(|k| k.id == 51).unwrap();
    assert_eq!(laptop.key_hash, "h51", "only device rows are marked");

    // One pairing token per device, kept as its hash.
    sqlx::query(
        "INSERT INTO device_pairing_tokens (key_id, token_hash, created_at, expires_at)
         VALUES (50, 't', '2026-10-15T10:00:00Z', '2026-10-15T10:10:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let again = sqlx::query(
        "INSERT INTO device_pairing_tokens (key_id, token_hash, created_at, expires_at)
         VALUES (50, 'u', '2026-10-15T10:00:00Z', '2026-10-15T10:10:00Z')",
    )
    .execute(&pool)
    .await;
    assert!(again.is_err(), "one token per device: {again:?}");

    // Two device rows never hold one enrolled key.
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, kind) VALUES (60, 'device:a', 'spki:ab', 'device')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let twin = sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, kind) VALUES (61, 'device:b', 'spki:ab', 'device')",
    )
    .execute(&pool)
    .await;
    assert!(twin.is_err(), "{twin:?}");

    // Every request row before it came in on the plain listener; the CHECK
    // holds the column to the two listeners.
    let via: String = sqlx::query_scalar("SELECT via FROM request_logs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(via, "plain");
    let bad = sqlx::query("UPDATE request_logs SET via = 'elsewhere'")
        .execute(&pool)
        .await;
    assert!(bad.is_err(), "{bad:?}");
}
